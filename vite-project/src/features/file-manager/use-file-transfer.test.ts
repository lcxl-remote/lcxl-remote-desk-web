import { renderHook, act, cleanup } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { useFileTransfer } from './use-file-transfer';
import { deskErrorCodeEnum } from '@/services';
import {
    CHUNK,
    installSignalingStubs,
    makeGatedFile,
    openChannel,
    parseSent,
    restoreSignalingStubs,
    flush,
    type SignalingGlobals,
} from './file-transfer-test-harness';

// Hook-level wiring for the W1/W7 upload fixes. The data channel is
// stubbed and we assert on the *frames written to it* — uploads must
// not push any binary chunk until the host accepts, and an accepted
// upload must stop when the host reports an error. One comprehensive
// test per file: see the note in `file-transfer-test-harness.ts`.

describe('useFileTransfer upload accept gating (W1/W7)', () => {
    let saved: SignalingGlobals;

    beforeEach(() => {
        saved = installSignalingStubs();
    });

    afterEach(() => {
        cleanup();
        restoreSignalingStubs(saved);
    });

    it('holds chunks until accept, then stops an accepted upload on transfer_error or refusal', async () => {
        const { result } = renderHook(() => useFileTransfer('desk-A'));
        const api = result.current;

        // --- Upload A: gate on accept, then stop mid-stream on error ---
        const fileA = makeGatedFile('a.bin', new Uint8Array(CHUNK).fill(1), new Uint8Array(CHUNK).fill(2));
        act(() => {
            void api.uploadFile('/dir', fileA.file);
        });
        const dc = await openChannel();

        // upload_request sent, but no bytes before the host accepts (W1).
        const reqA = parseSent(dc.textSent[0]);
        expect(reqA.type).toBe('upload_request');
        expect(dc.binarySent).toHaveLength(0);

        const tidA = reqA.transfer_id;
        act(() => dc.onmessage?.({ data: JSON.stringify({ type: 'upload_response', transfer_id: tidA, accepted: true }) }));
        await flush();
        // Accept released the loop; the first chunk is now on the wire.
        expect(dc.binarySent).toHaveLength(1);

        // Host errors out mid-stream — the loop must stop before chunk 2 (W7).
        act(() => dc.onmessage?.({ data: JSON.stringify({ type: 'transfer_error', transfer_id: tidA, message: 'disk full' }) }));
        fileA.release();
        await flush();
        expect(dc.binarySent).toHaveLength(1);

        // --- Upload B: refusal before accept sends zero bytes ---
        const binaryBefore = dc.binarySent.length;
        const textBefore = dc.textSent.length;
        const fileB = makeGatedFile('b.bin', new Uint8Array(CHUNK).fill(3), new Uint8Array(CHUNK).fill(4));
        let settledB = false;
        act(() => {
            void api.uploadFile('/dir', fileB.file).then(() => {
                settledB = true;
            });
        });
        await flush();
        const reqB = parseSent(dc.textSent[textBefore]);
        expect(reqB.type).toBe('upload_request');
        const tidB = reqB.transfer_id;

        act(() => dc.onmessage?.({ data: JSON.stringify({ type: 'transfer_error', transfer_id: tidB, error_code: deskErrorCodeEnum.SYSTEM_ERROR, message: 'no such dir' }) }));
        await flush();
        // No new binary frames, and the upload promise settled (the gate
        // rejected the waiter rather than hanging forever).
        expect(dc.binarySent.length).toBe(binaryBefore);
        expect(settledB).toBe(true);

        // --- Upload C: an empty file declares the zero chunks it sends ---
        // Claiming one chunk anyway left the host waiting for bytes that were
        // never coming, and it then rejected the upload as incomplete.
        const binaryBeforeC = dc.binarySent.length;
        const textBeforeC = dc.textSent.length;
        act(() => {
            void api.uploadFile('/dir', emptyFile('nothing.txt'));
        });
        await flush();

        const reqC = parseSent(dc.textSent[textBeforeC]);
        expect(reqC.type).toBe('upload_request');
        expect(reqC.file_size).toBe(0);
        expect(reqC.total_chunks).toBe(0);

        act(() => dc.onmessage?.({ data: JSON.stringify({ type: 'upload_response', transfer_id: reqC.transfer_id, accepted: true }) }));
        await flush();

        // Nothing to send, so it goes straight to declaring itself done.
        expect(dc.binarySent.length).toBe(binaryBeforeC);
        const completeC = dc.textSent
            .slice(textBeforeC)
            .map(parseSent)
            .find((m) => m.type === 'transfer_complete' && m.transfer_id === reqC.transfer_id);
        expect(completeC).toBeTruthy();
    });
});

/** A zero-byte file, whose stream is done on the first read. */
function emptyFile(name: string): File {
    return {
        name,
        size: 0,
        stream: () => ({
            getReader: () => ({
                async read() {
                    return { done: true, value: undefined };
                },
                cancel() {},
            }),
        }),
    } as unknown as File;
}
