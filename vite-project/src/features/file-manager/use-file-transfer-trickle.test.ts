import { renderHook, act, cleanup } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { useFileTransfer } from './use-file-transfer';
import {
    flush,
    installSignalingStubs,
    openSession,
    restoreSignalingStubs,
    sentSignalingOfType,
    StubPeerConnection,
    type SignalingGlobals,
} from './file-transfer-test-harness';

// The file manager negotiates with trickle ICE, like every other client of this
// host. It used to hold the offer until gathering completed, which made any slow
// or unreachable ICE server fatal: gathering does not finish until each
// configured server's allocation attempt has timed out, and the page gave up
// first. What this asserts is the ordering — offer out while gathering is still
// running, candidates following one frame at a time. One comprehensive
// `renderHook` test per file: see the note in `file-transfer-test-harness.ts`.

const OFFER = 102;
const ICE_CANDIDATE = 104;

const HOST_CANDIDATE = 'candidate:1 1 udp 2113937151 192.168.1.5 50000 typ host generation 0';
const RELAY_CANDIDATE = 'candidate:2 1 udp 41885695 203.0.113.7 50001 typ relay raddr 0.0.0.0 rport 0';

describe('useFileTransfer trickle ICE', () => {
    let saved: SignalingGlobals;

    beforeEach(() => {
        saved = installSignalingStubs();
    });

    afterEach(() => {
        cleanup();
        restoreSignalingStubs(saved);
    });

    it('sends the offer before gathering finishes, then trickles each candidate', async () => {
        const { result } = renderHook(() => useFileTransfer('desk-A'));
        act(() => {
            result.current.prepareTransfers();
        });
        const ws = await openSession({
            iceServers: [{ urls: 'turn:relay.example:3478', username: 'u', credential: 'c' }],
        });

        const pc = StubPeerConnection.last!;
        expect(pc).toBeTruthy();

        // The offer is already on the wire while gathering is still running.
        // This is the whole point: an ICE server that never answers can no
        // longer hold the negotiation hostage.
        expect(pc.iceGatheringState).toBe('gathering');
        const offers = sentSignalingOfType(ws, OFFER);
        expect(offers).toHaveLength(1);
        expect(offers[0].to_connection_id).toBe('desk-A');
        expect(offers[0].signaling_data.connection_epoch).toBe('test-epoch');
        expect(offers[0].signaling_data.offer).toEqual({ type: 'offer', sdp: 'stub' });
        // DataChannel-only offer: the settings field is present and explicitly null.
        expect(offers[0].signaling_data.session_settings).toBeNull();
        expect(sentSignalingOfType(ws, ICE_CANDIDATE)).toHaveLength(0);

        // Each gathered candidate goes out on its own, as it is produced.
        act(() => pc.emitCandidate(HOST_CANDIDATE));
        act(() => pc.emitCandidate(RELAY_CANDIDATE));
        await flush();

        const candidates = sentSignalingOfType(ws, ICE_CANDIDATE);
        expect(candidates).toHaveLength(2);
        expect(candidates[0].to_connection_id).toBe('desk-A');
        expect(candidates[0].signaling_data.connection_epoch).toBe('test-epoch');
        expect(candidates[0].signaling_data.candidate).toEqual({ candidate: HOST_CANDIDATE });
        expect(candidates[1].signaling_data.candidate).toEqual({ candidate: RELAY_CANDIDATE });

        // End-of-candidates is not a frame of its own: everything it used to
        // trigger has already been sent.
        act(() => pc.emitGatheringComplete());
        await flush();
        expect(sentSignalingOfType(ws, ICE_CANDIDATE)).toHaveLength(2);
        expect(sentSignalingOfType(ws, OFFER)).toHaveLength(1);
    });
});
