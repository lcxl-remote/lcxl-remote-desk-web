import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
    createIceRetryCoordinator,
    decideIceAction,
    iceStallWindowMs,
    type IceRetryDeps,
} from './ice-retry-coordinator';

describe('iceStallWindowMs', () => {
    it('grows exponentially from the base, capped at max', () => {
        expect(iceStallWindowMs(0, 1500, 6000)).toBe(1500);
        expect(iceStallWindowMs(1, 1500, 6000)).toBe(3000);
        expect(iceStallWindowMs(2, 1500, 6000)).toBe(6000);
        expect(iceStallWindowMs(3, 1500, 6000)).toBe(6000); // capped
        expect(iceStallWindowMs(10, 1500, 6000)).toBe(6000); // capped
    });
});

describe('decideIceAction', () => {
    const MAX = 3;

    it('reports connected on connected/completed regardless of budget', () => {
        expect(
            decideIceAction({ kind: 'ice-state', state: 'connected' }, 0, MAX),
        ).toBe('connected');
        expect(
            decideIceAction({ kind: 'ice-state', state: 'completed' }, MAX, MAX),
        ).toBe('connected');
    });

    it('waits on transient ICE states', () => {
        for (const state of ['new', 'checking', 'disconnected', 'closed'] as const) {
            expect(decideIceAction({ kind: 'ice-state', state }, 0, MAX)).toBe(
                'wait',
            );
        }
    });

    it('retries ICE on failed while budget remains, else fails', () => {
        expect(
            decideIceAction({ kind: 'ice-state', state: 'failed' }, 0, MAX),
        ).toBe('retry-ice');
        expect(
            decideIceAction({ kind: 'ice-state', state: 'failed' }, MAX, MAX),
        ).toBe('fail');
    });

    it('maps answer-timeout to action A (resend) until exhausted', () => {
        expect(decideIceAction({ kind: 'answer-timeout' }, 0, MAX)).toBe(
            'retry-answer',
        );
        expect(decideIceAction({ kind: 'answer-timeout' }, MAX, MAX)).toBe('fail');
    });

    it('maps ice-stall to action B (ice restart) until exhausted', () => {
        expect(decideIceAction({ kind: 'ice-stall' }, 1, MAX)).toBe('retry-ice');
        expect(decideIceAction({ kind: 'ice-stall' }, MAX, MAX)).toBe('fail');
    });
});

describe('createIceRetryCoordinator', () => {
    const ANSWER_MS = 5000;
    const ICE_MS = 5000; // base stall window
    const ICE_MAX_MS = 20000; // backoff cap (well above base so growth is observable)
    const MAX_RETRY = 3;

    function makeCoordinator(overrides: Partial<IceRetryDeps> = {}) {
        let idCounter = 0;
        const resend = vi.fn((id: string, onSent: (id: string) => void) => {
            onSent(id);
        });
        const iceRestart = vi.fn(
            async (id: string, onSent: (id: string) => void) => {
                onSent(id);
            },
        );
        const onConnected = vi.fn();
        const onFailed = vi.fn();
        const deps: IceRetryDeps = {
            resendCachedOffer: resend,
            sendIceRestartOffer: iceRestart,
            onConnected,
            onFailed,
            genRequestId: () => `req-${++idCounter}`,
            config: {
                answerTimeoutMs: ANSWER_MS,
                iceStallBaseMs: ICE_MS,
                iceStallMaxMs: ICE_MAX_MS,
                maxRetry: MAX_RETRY,
            },
            ...overrides,
        };
        const coord = createIceRetryCoordinator(deps);
        return { coord, resend, iceRestart, onConnected, onFailed };
    }

    beforeEach(() => {
        vi.useFakeTimers();
    });
    afterEach(() => {
        vi.useRealTimers();
        vi.restoreAllMocks();
    });

    it('arms the ANSWER watchdog only once the OFFER is actually sent', async () => {
        const { coord, resend } = makeCoordinator();
        coord.resetForNewPc();
        coord.beginOffer(); // allocates, but NOT yet sent
        // No markOfferSent yet → no timer armed → advancing time does nothing.
        await vi.advanceTimersByTimeAsync(ANSWER_MS * 2);
        expect(resend).not.toHaveBeenCalled();
        expect(coord.snapshot().retryCount).toBe(0);
    });

    it('resends the cached offer when no ANSWER arrives in time (action A)', async () => {
        const { coord, resend, iceRestart } = makeCoordinator();
        coord.resetForNewPc();
        const id1 = coord.beginOffer();
        coord.markOfferSent(id1);

        await vi.advanceTimersByTimeAsync(ANSWER_MS - 1);
        expect(resend).not.toHaveBeenCalled();
        await vi.advanceTimersByTimeAsync(1);

        expect(resend).toHaveBeenCalledTimes(1);
        expect(iceRestart).not.toHaveBeenCalled();
        expect(coord.snapshot().retryCount).toBe(1);
        // The resend (mock fires onSent) re-armed a fresh ANSWER watchdog.
        expect(coord.snapshot().phase).toBe('awaiting-answer');
    });

    it('restarts ICE when checking stalls after a matching ANSWER (action B)', async () => {
        const { coord, resend, iceRestart } = makeCoordinator();
        coord.resetForNewPc();
        const id1 = coord.beginOffer();
        coord.markOfferSent(id1);
        coord.onAnswerApplied('ufrag-1'); // -> checking

        await vi.advanceTimersByTimeAsync(ICE_MS);
        expect(iceRestart).toHaveBeenCalledTimes(1);
        expect(resend).not.toHaveBeenCalled();
        expect(coord.snapshot().retryCount).toBe(1);
    });

    it('widens the stall window on later attempts (capped backoff)', async () => {
        const { coord, iceRestart } = makeCoordinator();
        coord.resetForNewPc();
        const id1 = coord.beginOffer();
        coord.markOfferSent(id1);
        coord.onAnswerApplied('ufrag-1'); // attempt 0 -> stall window = base

        // First stall fires at the base window.
        await vi.advanceTimersByTimeAsync(ICE_MS);
        expect(iceRestart).toHaveBeenCalledTimes(1);

        // The retry's offer is "sent"; apply its answer to re-enter checking.
        // retryCount is now 1, so the next stall window is 2x base.
        coord.onAnswerApplied('ufrag-2');
        await vi.advanceTimersByTimeAsync(ICE_MS);
        expect(iceRestart).toHaveBeenCalledTimes(1); // base elapsed, but window is 2x
        await vi.advanceTimersByTimeAsync(ICE_MS);
        expect(iceRestart).toHaveBeenCalledTimes(2); // 2x base reached -> restart
    });

    it('gates ANSWER by the pending OFFER request_id', () => {
        const { coord } = makeCoordinator();
        coord.resetForNewPc();
        const id1 = coord.beginOffer();
        expect(coord.shouldAcceptAnswer(id1)).toBe(true);
        expect(coord.shouldAcceptAnswer('some-stale-id')).toBe(false);
    });

    it('queues candidates before ANSWER, then accepts only the matching ufrag', () => {
        const { coord } = makeCoordinator();
        coord.resetForNewPc();
        const id1 = coord.beginOffer();
        coord.markOfferSent(id1);
        // awaiting-answer: everything queues (ufrag unknown).
        expect(coord.classifyCandidate('ufrag-1')).toBe('queue');

        coord.onAnswerApplied('ufrag-1'); // -> checking, currentRemoteUfrag=ufrag-1
        expect(coord.classifyCandidate('ufrag-1')).toBe('apply');
        expect(coord.classifyCandidate(null)).toBe('apply'); // untagged -> current
        expect(coord.classifyCandidate('ufrag-OLD')).toBe('reject');
    });

    it('serializes concurrent retry triggers into a single retry', () => {
        // A send that arms the next watchdog but never resolves keeps
        // `retryInFlight` set, so a second trigger arriving while the first
        // retry is still in flight is a no-op.
        let release: () => void = () => {};
        const pending = new Promise<void>((r) => {
            release = r;
        });
        const iceRestart = vi.fn(
            async (id: string, onSent: (id: string) => void) => {
                onSent(id);
                await pending;
            },
        );
        const { coord } = makeCoordinator({ sendIceRestartOffer: iceRestart });
        coord.resetForNewPc();
        const id1 = coord.beginOffer();
        coord.markOfferSent(id1);
        coord.onAnswerApplied('ufrag-1');

        // Two ICE `failed` callbacks back-to-back (e.g. failed racing the
        // stall timer): the first holds the in-flight guard, the second is
        // dropped.
        coord.onIceStateChange('failed');
        coord.onIceStateChange('failed');
        expect(iceRestart).toHaveBeenCalledTimes(1);
        expect(coord.snapshot().retryCount).toBe(1);
        release();
    });

    it('still honors a genuine connected after a retry bumped the generation (pcEpoch vs offerGeneration)', async () => {
        const { coord, onConnected } = makeCoordinator();
        coord.resetForNewPc();
        const id1 = coord.beginOffer();
        coord.markOfferSent(id1);
        coord.onAnswerApplied('ufrag-1');

        // Force an ICE restart (generation 1 -> 2).
        await vi.advanceTimersByTimeAsync(ICE_MS);
        expect(coord.snapshot().generation).toBeGreaterThan(1);

        // The SAME PeerConnection later connects: it must be honored, not
        // discarded as a stale generation.
        coord.onIceStateChange('connected');
        expect(onConnected).toHaveBeenCalledTimes(1);
        expect(coord.snapshot().phase).toBe('connected');
        expect(coord.snapshot().retryCount).toBe(0);
    });

    it('surfaces terminal failure after exhausting the retry budget', async () => {
        const { coord, onFailed } = makeCoordinator();
        coord.resetForNewPc();
        const id1 = coord.beginOffer();
        coord.markOfferSent(id1);

        // Each answer-timeout resends and re-arms; after MAX_RETRY retries the
        // next timeout fails.
        for (let i = 0; i < MAX_RETRY; i++) {
            await vi.advanceTimersByTimeAsync(ANSWER_MS);
        }
        expect(coord.snapshot().retryCount).toBe(MAX_RETRY);
        expect(onFailed).not.toHaveBeenCalled();
        await vi.advanceTimersByTimeAsync(ANSWER_MS);
        expect(onFailed).toHaveBeenCalledTimes(1);
        expect(coord.snapshot().phase).toBe('failed');
    });

    it('fails visibly when an ICE-restart offer send throws', async () => {
        const iceRestart = vi.fn(async () => {
            throw new Error('createOffer failed');
        });
        const { coord, onFailed } = makeCoordinator({
            sendIceRestartOffer: iceRestart,
        });
        coord.resetForNewPc();
        const id1 = coord.beginOffer();
        coord.markOfferSent(id1);
        coord.onAnswerApplied('ufrag-1');

        await vi.advanceTimersByTimeAsync(ICE_MS);
        expect(iceRestart).toHaveBeenCalledTimes(1);
        expect(onFailed).toHaveBeenCalledTimes(1);
        expect(coord.snapshot().phase).toBe('failed');
    });

    it('does not retry after dispose, and bumps the epoch', async () => {
        const { coord, resend } = makeCoordinator();
        coord.resetForNewPc();
        const epochBefore = coord.currentEpoch();
        const id1 = coord.beginOffer();
        coord.markOfferSent(id1);

        coord.dispose();
        expect(coord.currentEpoch()).toBe(epochBefore + 1);

        await vi.advanceTimersByTimeAsync(ANSWER_MS * 2);
        expect(resend).not.toHaveBeenCalled();
    });
});
