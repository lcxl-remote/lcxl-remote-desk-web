import { afterEach, describe, expect, it, vi } from 'vitest';
import { UploadController, UPLOAD_CHUNK_SIZE, type UploadProgress } from './upload-controller';
import { deskErrorCodeEnum } from '@/services/types';

export class UploadTestChannel extends EventTarget {
    readyState = 'open';
    bufferedAmount = 0;
    sent: (string | ArrayBuffer)[] = [];
    send(data: string | ArrayBuffer) { this.sent.push(data); }
    message(message: object) { this.dispatchEvent(new MessageEvent('message', { data: JSON.stringify(message) })); }
    get texts() { return this.sent.filter((data): data is string => typeof data === 'string').map(data => JSON.parse(data)); }
    get chunks() { return this.sent.filter(data => typeof data !== 'string') as ArrayBuffer[]; }
    close() { this.readyState = 'closed'; this.dispatchEvent(new Event('close')); }
}

export function uploadTestFile(name = 'report.txt', size = 0): File {
    return { name, size, stream: () => new ReadableStream<Uint8Array>({ start(controller) {
        if (size) controller.enqueue(new Uint8Array(size).fill(7));
        controller.close();
    } }) } as File;
}

const flush = async () => { for (let i = 0; i < 16; i++) await Promise.resolve(); };
const controllers: UploadController[] = [];
afterEach(() => { controllers.splice(0).forEach(controller => controller.dispose()); vi.useRealTimers(); });

function setup(timeout = 30_000) {
    const channel = new UploadTestChannel();
    const rows: UploadProgress[] = [];
    const controller = new UploadController(channel as unknown as RTCDataChannel, row => rows.push(row), timeout);
    controllers.push(controller);
    return { channel, rows, controller };
}

describe('channel uploads', () => {
    it.each([0, UPLOAD_CHUNK_SIZE * 2 + 3])('waits for acceptance and durable confirmation for %i bytes', async size => {
        const { controller, channel, rows } = setup();
        let ended = false;
        const result = controller.upload(uploadTestFile('report.txt', size), { target_location: 'downloads' }).then(row => { ended = true; return row; });
        const request = channel.texts[0];
        expect(request.target_location).toBe('downloads');
        expect(request.target_dir).toBeUndefined();
        expect(request.total_chunks).toBe(Math.ceil(size / UPLOAD_CHUNK_SIZE));
        expect(channel.chunks).toHaveLength(0);
        const id = request.transfer_id;
        channel.message({ type: 'transfer_complete', transfer_id: id });
        expect(ended).toBe(false);
        channel.message({ type: 'upload_response', transfer_id: id, accepted: true, resolved_target_dir: '/downloads', resolved_file_name: 'report (1).txt' });
        await flush();
        expect(rows.at(-1)?.status).toBe('confirming');
        expect(ended).toBe(false);
        expect(channel.chunks).toHaveLength(request.total_chunks);
        channel.chunks.forEach(chunk => expect(chunk.byteLength).toBeLessThanOrEqual(65536));
        const endFrame = channel.texts.find(message => message.type === 'transfer_complete');
        expect(endFrame).not.toHaveProperty('resolved_file_name');
        expect(endFrame).not.toHaveProperty('resolved_target_dir');
        controller.cancel(id);
        expect(channel.texts.some(message => message.type === 'transfer_cancel')).toBe(false);
        channel.message({ type: 'transfer_complete', transfer_id: id, resolved_file_name: 'report (2).txt', resolved_target_dir: '/downloads' });
        expect((await result).status).toBe('completed');
        expect(rows.at(-1)?.resolvedFileName).toBe('report (2).txt');
        channel.message({ type: 'transfer_error', transfer_id: id, error_code: deskErrorCodeEnum.SYSTEM_ERROR, message: 'late' });
        expect(rows.at(-1)?.status).toBe('completed');
    });

    it('keeps a long human approval alive, then cancels a genuinely silent request', async () => {
        vi.useFakeTimers();
        const { controller, channel, rows } = setup();
        const stream = vi.fn(() => uploadTestFile().stream());
        const result = controller.upload({ name: 'empty', size: 0, stream } as unknown as File, { target_dir: '/folder' });
        const id = channel.texts[0].transfer_id;
        for (let second = 0; second < 7; second++) {
            await vi.advanceTimersByTimeAsync(10_000);
            channel.message({ type: 'upload_state_changed', transfer_id: id, phase: 'awaiting_approval' });
        }
        expect(rows.at(-1)?.status).toBe('awaiting_approval');
        expect(stream).not.toHaveBeenCalled();
        await vi.advanceTimersByTimeAsync(30_001);
        expect((await result).status).toBe('error');
        expect(channel.texts.filter(message => message.type === 'transfer_cancel')).toHaveLength(1);
        channel.message({ type: 'upload_response', transfer_id: id, accepted: true });
        await flush();
        expect(stream).not.toHaveBeenCalled();
    });

    it('cancels under backpressure and does not send another chunk', async () => {
        vi.useFakeTimers();
        const { controller, channel } = setup();
        channel.bufferedAmount = 1024 * 1024;
        const result = controller.upload(uploadTestFile('large', UPLOAD_CHUNK_SIZE * 2), { target_dir: '/folder' });
        const id = channel.texts[0].transfer_id;
        channel.message({ type: 'upload_response', transfer_id: id, accepted: true });
        await flush();
        expect(channel.chunks).toHaveLength(1);
        controller.cancel(id);
        await vi.advanceTimersByTimeAsync(100);
        expect((await result).status).toBe('cancelled');
        expect(channel.chunks).toHaveLength(1);
        expect(channel.texts.filter(message => message.type === 'transfer_cancel')).toHaveLength(1);
    });

    it.each(['timeout', 'close', 'host-error'])('reports an unconfirmed result after sending when %s occurs', async cause => {
        vi.useFakeTimers();
        const { controller, channel, rows } = setup(1000);
        const result = controller.upload(uploadTestFile(), { target_location: 'downloads' });
        const id = channel.texts[0].transfer_id;
        channel.message({ type: 'upload_response', transfer_id: id, accepted: true });
        await flush();
        if (cause === 'close') channel.close();
        else if (cause === 'host-error') channel.message({ type: 'transfer_error', transfer_id: id, error_code: deskErrorCodeEnum.FILE_SAVE_UNCONFIRMED, message: 'directory sync failed' });
        else await vi.advanceTimersByTimeAsync(1001);
        expect((await result).status).toBe('unconfirmed');
        expect(channel.texts.filter(message => message.type === 'upload_request')).toHaveLength(1);
        channel.message({ type: 'transfer_complete', transfer_id: id });
        expect(rows.at(-1)?.status).toBe('unconfirmed');
    });
});
