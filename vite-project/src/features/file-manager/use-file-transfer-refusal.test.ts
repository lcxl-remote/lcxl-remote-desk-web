import { renderHook, act, cleanup } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { deskErrorCodeEnum } from '@/services/types';
import { useFileTransfer } from './use-file-transfer';
import { TRANSFER_INACTIVITY_TIMEOUT_MS } from './transfer-registry';
import {
    binaryChunk,
    installSignalingStubs,
    openChannel,
    parseSent,
    restoreSignalingStubs,
    flush,
    type SignalingGlobals,
} from './file-transfer-test-harness';

// Hook-level wiring for a download the host never delivers. Three ways that
// happens — an explicit refusal, total silence, and a stream that stops
// partway — must all end the transfer, and nothing arriving afterwards may
// restart it. Asserted on the frames written to the channel and on the
// destination writable, which is what the harness can observe. One
// comprehensive test per file: see the note in `file-transfer-test-harness.ts`.

function fakeWritable() {
    const w = {
        written: [] as Uint8Array[],
        closed: false,
        aborted: false,
        write: vi.fn(async (d: Uint8Array) => {
            w.written.push(d);
        }),
        close: vi.fn(async () => {
            w.closed = true;
        }),
        abort: vi.fn(async () => {
            w.aborted = true;
        }),
    };
    return w;
}

const cancelsFor = (frames: string[], transferId: string) =>
    frames.map(parseSent).filter((m) => m.type === 'transfer_cancel' && m.transfer_id === transferId);

describe('useFileTransfer download refusal and stalls', () => {
    let saved: SignalingGlobals;
    let pickerQueue: unknown[];

    beforeEach(() => {
        saved = installSignalingStubs();
        pickerQueue = [];
        (window as any).showSaveFilePicker = vi.fn(async () => ({
            createWritable: async () => pickerQueue.shift(),
        }));
    });

    afterEach(() => {
        cleanup();
        restoreSignalingStubs(saved);
        delete (window as any).showSaveFilePicker;
        vi.useRealTimers();
        vi.restoreAllMocks();
    });

    it('ends a refused, a silent and a stalled download, and ignores everything that arrives after', async () => {
        const refusedFile = fakeWritable();
        const silentFile = fakeWritable();
        const stalledFile = fakeWritable();
        pickerQueue.push(refusedFile, silentFile, stalledFile);

        const { result } = renderHook(() => useFileTransfer('desk-A'));
        const api = result.current;

        // --- A refusal releases the file handle instead of hanging ---
        act(() => {
            void api.downloadFile('/remote/a.bin', 'a.bin');
        });
        const dc = await openChannel();
        const refused = parseSent(dc.textSent[0]).transfer_id;

        act(() =>
            dc.onmessage?.({
                data: JSON.stringify({
                    type: 'transfer_error',
                    transfer_id: refused,
                    error_code: deskErrorCodeEnum.PERMISSION_ERROR,
                    message: 'File transfer is not permitted on this connection',
                }),
            }),
        );
        await flush();
        expect(refusedFile.aborted).toBe(true);

        // Anything arriving for that id afterwards is no longer ours: a late
        // response must not rebuild a sink, and a late chunk must not be
        // written to a file the user was already told about.
        act(() =>
            dc.onmessage?.({
                data: JSON.stringify({
                    type: 'download_response',
                    transfer_id: refused,
                    file_name: 'a.bin',
                    file_size: 4,
                    chunk_size: 2,
                    total_chunks: 2,
                }),
            }),
        );
        act(() => dc.onmessage?.({ data: binaryChunk(refused, 0, new Uint8Array([1, 2])) }));
        act(() =>
            dc.onmessage?.({
                data: JSON.stringify({ type: 'transfer_complete', transfer_id: refused }),
            }),
        );
        await flush();
        expect(refusedFile.write).not.toHaveBeenCalled();
        expect(refusedFile.closed).toBe(false);

        // --- A host that answers nothing at all is given up on ---
        vi.useFakeTimers();
        const beforeSilent = dc.textSent.length;
        act(() => {
            void api.downloadFile('/remote/b.bin', 'b.bin');
        });
        await act(async () => {
            await vi.advanceTimersByTimeAsync(0);
        });
        const silent = parseSent(dc.textSent[beforeSilent]).transfer_id;

        await act(async () => {
            await vi.advanceTimersByTimeAsync(TRANSFER_INACTIVITY_TIMEOUT_MS - 1);
        });
        expect(cancelsFor(dc.textSent, silent)).toHaveLength(0);

        await act(async () => {
            await vi.advanceTimersByTimeAsync(1);
        });
        // Giving up asks the host to stop, in case it is merely slow, and
        // releases the destination file.
        expect(cancelsFor(dc.textSent, silent)).toHaveLength(1);
        expect(silentFile.aborted).toBe(true);

        // --- A stream that starts and then stops is given up on too ---
        const beforeStalled = dc.textSent.length;
        act(() => {
            void api.downloadFile('/remote/c.bin', 'c.bin');
        });
        await act(async () => {
            await vi.advanceTimersByTimeAsync(0);
        });
        const stalled = parseSent(dc.textSent[beforeStalled]).transfer_id;

        act(() =>
            dc.onmessage?.({
                data: JSON.stringify({
                    type: 'download_response',
                    transfer_id: stalled,
                    file_name: 'c.bin',
                    file_size: 8,
                    chunk_size: 2,
                    total_chunks: 4,
                }),
            }),
        );
        // Arriving data keeps pushing the deadline out, so a slow-but-alive
        // transfer is never killed.
        for (let chunk = 0; chunk < 3; chunk++) {
            await act(async () => {
                await vi.advanceTimersByTimeAsync(TRANSFER_INACTIVITY_TIMEOUT_MS - 1);
            });
            act(() =>
                dc.onmessage?.({ data: binaryChunk(stalled, chunk, new Uint8Array([1, 2])) }),
            );
        }
        await act(async () => {
            await vi.advanceTimersByTimeAsync(0);
        });
        expect(cancelsFor(dc.textSent, stalled)).toHaveLength(0);
        expect(stalledFile.write).toHaveBeenCalledTimes(3);

        // Then the host goes quiet with the file unfinished.
        await act(async () => {
            await vi.advanceTimersByTimeAsync(TRANSFER_INACTIVITY_TIMEOUT_MS);
        });
        expect(cancelsFor(dc.textSent, stalled)).toHaveLength(1);
        expect(stalledFile.aborted).toBe(true);
        expect(stalledFile.closed).toBe(false);

        // A completion racing in behind the timeout must not reopen it.
        act(() =>
            dc.onmessage?.({
                data: JSON.stringify({ type: 'transfer_complete', transfer_id: stalled }),
            }),
        );
        await act(async () => {
            await vi.advanceTimersByTimeAsync(0);
        });
        expect(stalledFile.closed).toBe(false);
        expect(cancelsFor(dc.textSent, stalled)).toHaveLength(1);
    });
});
