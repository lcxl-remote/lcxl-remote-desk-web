import { renderHook, act, cleanup } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { useFileTransfer } from './use-file-transfer';
import {
    deliverSignaling,
    flush,
    installSignalingStubs,
    openSession,
    restoreSignalingStubs,
    sentSignalingOfType,
    StubDataChannel,
    StubPeerConnection,
    type SignalingGlobals,
} from './file-transfer-test-harness';

// Regression test for the production failure this whole split came from.
//
// A TURN relay the browser could not reach kept ICE gathering running for as
// long as the browser was willing to retry its allocation. The page waited for
// gathering to finish before sending its offer, and waited for the data channel
// before listing anything — so it sent one frame, then nothing, and closed the
// socket 20 seconds later. The directory the user asked for never even left the
// browser.
//
// The stall itself is reproduced exactly (gathering never completes). What must
// no longer follow from it: the offer must already be out, and the listing must
// complete on the signaling session alone. One comprehensive `renderHook` test
// per file: see the note in `file-transfer-test-harness.ts`.

const OFFER = 102;
const LIST_FILES = 10005;
const FILES_LISTED = 10015;

describe('useFileTransfer with a gathering phase that never ends', () => {
    let saved: SignalingGlobals;

    beforeEach(() => {
        saved = installSignalingStubs();
    });

    afterEach(() => {
        cleanup();
        restoreSignalingStubs(saved);
    });

    it('still sends the offer and still lists files', async () => {
        const { result } = renderHook(() => useFileTransfer('desk-A'));

        let listed: unknown = null;
        act(() => {
            void result.current.listFiles({ path: '', page_no: 1, page_count: 100 }).then((response) => {
                listed = response;
            });
        });
        act(() => {
            result.current.prepareTransfers();
        });

        const ws = await openSession({ iceServers: [{ urls: 'turn:unreachable.example:3478' }] });

        // The stall, reproduced: no end-of-candidates will ever arrive.
        const pc = StubPeerConnection.last!;
        expect(pc.iceGatheringState).toBe('gathering');
        // The offer went out anyway — this is the frame that never left in
        // production.
        expect(sentSignalingOfType(ws, OFFER)).toHaveLength(1);

        // And the listing, which needs nothing but the session, completes while
        // the data channel is still stuck.
        const ask = sentSignalingOfType(ws, LIST_FILES)[0];
        expect(ask).toBeTruthy();
        expect(ask.to_connection_id).toBe('desk-A');
        await deliverSignaling({
            request_id: ask.request_id,
            signaling_type: FILES_LISTED,
            signaling_data: { file_info_list: [{ name: 'a.txt' }], total_count: 1 },
        });
        await flush();

        expect(listed).toEqual({ file_info_list: [{ name: 'a.txt' }], total_count: 1 });
        // Nothing about the listing depended on the channel: it is still not open.
        expect(StubDataChannel.last!.readyState).toBe('connecting');
        // One socket, still up. In production this had been closed by now.
        expect(ws.readyState).toBe(1);
    });
});
