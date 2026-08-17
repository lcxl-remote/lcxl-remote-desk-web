import { renderHook, act, cleanup } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { DATA_CHANNEL_TIMEOUT_MS, useFileTransfer } from './use-file-transfer';
import {
    advanceTime,
    installSignalingStubs,
    openSession,
    restoreSignalingStubs,
    StubPeerConnection,
    StubWebSocket,
    type SignalingGlobals,
} from './file-transfer-test-harness';

// Connection ownership rules, asserted where they used to be violated.
//
// Reconnecting once opened a second WebSocket without closing the first, and
// replacing a peer connection overwrote the reference without closing the old
// one — so a page that retried a few times left sockets and peer connections
// running behind it, still bound to their callbacks. With the session and the
// channel now having separate lifetimes there are more replacements, not fewer,
// so the rules are worth pinning down: one socket for the page, and every
// replaced peer connection closed. One comprehensive `renderHook` test per file:
// see the note in `file-transfer-test-harness.ts`.

describe('useFileTransfer connection ownership', () => {
    let saved: SignalingGlobals;

    beforeEach(() => {
        saved = installSignalingStubs();
        // Installed before anything is scheduled, so the channel timeouts these
        // retries hang off are armed against this clock.
        vi.useFakeTimers();
    });

    afterEach(() => {
        vi.useRealTimers();
        cleanup();
        restoreSignalingStubs(saved);
    });

    it('reuses one socket across channel retries, closes every replaced peer connection, and closes idempotently', async () => {
        const { result } = renderHook(() => useFileTransfer('desk-A'));
        act(() => {
            result.current.prepareTransfers();
        });
        await openSession({ iceServers: [{ urls: 'turn:unreachable.example:3478' }] });

        expect(StubWebSocket.instances).toHaveLength(1);
        expect(StubPeerConnection.instances).toHaveLength(1);

        // Three failed attempts, each retried the way the banner's button does.
        for (let round = 0; round < 3; round++) {
            await advanceTime(DATA_CHANNEL_TIMEOUT_MS);
            expect(result.current.channelStatus).toBe('failed');
            act(() => {
                result.current.prepareTransfers();
            });
            await advanceTime(0);
        }
        await advanceTime(DATA_CHANNEL_TIMEOUT_MS);

        // One socket for the whole page. A retry re-uses the live session rather
        // than opening — and orphaning — another one.
        expect(StubWebSocket.instances).toHaveLength(1);
        expect(StubWebSocket.instances[0].readyState).toBe(1);

        // Every attempt built its own peer connection, and none of them is still
        // running.
        expect(StubPeerConnection.instances.length).toBeGreaterThan(1);
        expect(StubPeerConnection.instances.every((pc) => pc.closed)).toBe(true);

        // Each attempt reports its own candidates, not an accumulation of every
        // attempt so far.
        expect(result.current.channelFailure?.diagnostics.candidateCounts.host).toBe(0);

        // Closing twice must not throw, and must leave the same state.
        act(() => {
            result.current.closeConnection();
            result.current.closeConnection();
        });
        expect(result.current.channelStatus).toBe('idle');
        expect(result.current.channelFailure).toBeNull();
        expect(StubWebSocket.instances[0].readyState).toBe(3);
    });
});
