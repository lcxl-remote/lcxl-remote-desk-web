import { renderHook, act, cleanup } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useFileTransfer } from './use-file-transfer';
import {
    advanceTime,
    deliverSignaling,
    installSignalingStubs,
    openSession,
    restoreSignalingStubs,
    sentSignalingOfType,
    StubPeerConnection,
    type SignalingGlobals,
} from './file-transfer-test-harness';

// ICE giving up used to be a `console.warn` and nothing else, so a connection
// that had definitively failed still sat there until the page's own timeout
// expired — and then reported that timeout instead of the failure.
//
// A failed negotiation is terminal, so it ends the attempt at once, with the
// evidence that distinguishes it from a stalled gathering: candidates were
// gathered and complete, and connectivity still could not be established. One
// comprehensive `renderHook` test per file: see the note in
// `file-transfer-test-harness.ts`.

const LIST_FILES = 10005;
const FILES_LISTED = 10015;

describe('useFileTransfer when ICE fails', () => {
    let saved: SignalingGlobals;

    beforeEach(() => {
        saved = installSignalingStubs();
        vi.useFakeTimers();
    });

    afterEach(() => {
        vi.useRealTimers();
        cleanup();
        restoreSignalingStubs(saved);
    });

    it('ends the attempt at once, names the cause, and leaves the session usable', async () => {
        const { result } = renderHook(() => useFileTransfer('desk-A'));
        act(() => {
            result.current.prepareTransfers();
        });
        const ws = await openSession({
            iceServers: [{ urls: 'turn:relay.example:3478', username: 'u', credential: 'c' }],
        });

        const pc = StubPeerConnection.last!;
        act(() => pc.emitCandidate('candidate:1 1 udp 2113937151 192.168.1.5 50000 typ host'));
        act(() => pc.emitCandidate('candidate:2 1 udp 41885695 203.0.113.7 50001 typ relay raddr 0.0.0.0 rport 0'));
        act(() => pc.emitGatheringComplete());
        await advanceTime(0);

        act(() => pc.setIceConnectionState('failed'));
        await advanceTime(0);

        expect(result.current.channelStatus).toBe('failed');
        expect(result.current.channelFailure?.kind).toBe('ice-failed');

        // The evidence separates this from a relay that never answered: the relay
        // did answer, gathering finished, and connectivity still failed.
        const diagnostics = result.current.channelFailure!.diagnostics;
        expect(diagnostics.candidateCounts.relay).toBe(1);
        expect(diagnostics.gatheringState).toBe('complete');
        expect(diagnostics.iceConnectionState).toBe('failed');

        expect(pc.closed).toBe(true);
        // No timeout was ever needed, and none is left armed for the channel.
        expect(ws.readyState).toBe(1);

        // Browsing continues over the surviving session.
        let listed: unknown = null;
        act(() => {
            void result.current.listFiles({ path: '/', page_no: 1, page_count: 100 }).then((response) => {
                listed = response;
            });
        });
        await advanceTime(0);
        const ask = sentSignalingOfType(ws, LIST_FILES).at(-1);
        await deliverSignaling({
            request_id: ask.request_id,
            signaling_type: FILES_LISTED,
            signaling_data: { file_info_list: [], total_count: 0 },
        });
        await advanceTime(0);
        expect(listed).toEqual({ file_info_list: [], total_count: 0 });
    });
});
