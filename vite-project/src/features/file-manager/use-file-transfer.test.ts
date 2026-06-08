import { renderHook, act, cleanup } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { useFileTransfer } from './use-file-transfer';
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

        act(() => dc.onmessage?.({ data: JSON.stringify({ type: 'transfer_error', transfer_id: tidB, message: 'no such dir' }) }));
        await flush();
        // No new binary frames, and the upload promise settled (the gate
        // rejected the waiter rather than hanging forever).
        expect(dc.binarySent.length).toBe(binaryBefore);
        expect(settledB).toBe(true);
    });
});
