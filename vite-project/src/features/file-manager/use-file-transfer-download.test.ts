import { renderHook, act, cleanup } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useFileTransfer } from './use-file-transfer';
import {
    binaryChunk,
    installSignalingStubs,
    openChannel,
    parseSent,
    restoreSignalingStubs,
    flush,
    type SignalingGlobals,
} from './file-transfer-test-harness';

// Hook-level wiring for the W2/W6 download fixes. Downloads must stream
// straight to the destination writable (never buffered in memory) and a
// write failure must abort the transfer and tell the host to stop — all
// without leaking an unhandled rejection. One comprehensive test per
// file: see the note in `file-transfer-test-harness.ts`.

function fakeWritable(opts: { failWrite?: boolean; failAbort?: boolean } = {}) {
    const w = {
        written: [] as Uint8Array[],
        closed: false,
        aborted: false,
        write: vi.fn(async (d: Uint8Array) => {
            if (opts.failWrite) throw new Error('disk full');
            w.written.push(d);
        }),
        close: vi.fn(async () => {
            w.closed = true;
        }),
        abort: vi.fn(async () => {
            w.aborted = true;
            if (opts.failAbort) throw new Error('abort also failed');
        }),
    };
    return w;
}

describe('useFileTransfer streaming download (W2/W6)', () => {
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
        vi.restoreAllMocks();
    });

    it('streams to disk and, on write failure, aborts + tells the host to stop with no unhandled rejection', async () => {
        const unhandled = vi.fn();
        process.on('unhandledRejection', unhandled);
        try {
            const ok = fakeWritable();
            const bad = fakeWritable({ failWrite: true, failAbort: true });
            pickerQueue.push(ok, bad);

            const { result } = renderHook(() => useFileTransfer('desk-A'));
            const api = result.current;

            // --- Download A: streams chunks straight to the writable (W2) ---
            act(() => {
                void api.downloadFile('/remote/a.bin', 'a.bin');
            });
            const dc = await openChannel();
            const tidA = parseSent(dc.textSent[0]).transfer_id;

            act(() =>
                dc.onmessage?.({
                    data: JSON.stringify({
                        type: 'download_response',
                        transfer_id: tidA,
                        file_name: 'a.bin',
                        file_size: 4,
                        chunk_size: 2,
                        total_chunks: 2,
                    }),
                }),
            );
            act(() => dc.onmessage?.({ data: binaryChunk(tidA, 0, new Uint8Array([1, 2])) }));
            act(() => dc.onmessage?.({ data: binaryChunk(tidA, 1, new Uint8Array([3, 4])) }));
            await flush();
            act(() => dc.onmessage?.({ data: JSON.stringify({ type: 'transfer_complete', transfer_id: tidA }) }));
            await flush();

            expect(ok.written.flatMap((c) => Array.from(c))).toEqual([1, 2, 3, 4]);
            expect(ok.closed).toBe(true);

            // --- Download B: a failing write aborts + cancels (W6) ---
            const textBefore = dc.textSent.length;
            act(() => {
                void api.downloadFile('/remote/b.bin', 'b.bin');
            });
            await flush();
            const tidB = parseSent(dc.textSent[textBefore]).transfer_id;

            act(() =>
                dc.onmessage?.({
                    data: JSON.stringify({
                        type: 'download_response',
                        transfer_id: tidB,
                        file_name: 'b.bin',
                        file_size: 2,
                        chunk_size: 2,
                        total_chunks: 1,
                    }),
                }),
            );
            act(() => dc.onmessage?.({ data: binaryChunk(tidB, 0, new Uint8Array([9, 9])) }));
            await flush();

            const cancel = dc.textSent.map(parseSent).find((m) => m.type === 'transfer_cancel' && m.transfer_id === tidB);
            expect(cancel).toBeTruthy();
            expect(bad.abort).toHaveBeenCalled(); // best-effort abort attempted
            expect(unhandled).not.toHaveBeenCalled(); // failing abort did not bubble
        } finally {
            process.off('unhandledRejection', unhandled);
        }
    });
});
