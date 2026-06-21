/**
 * Self-healing ICE negotiation for the browser (controlling) side.
 *
 * A WebRTC connection that stalls during `checking` would otherwise dead-
 * wait the server's ~20s ICE-failed timeout and then surface a manual
 * retry — even though a fresh attempt almost always connects in well under
 * a second. This coordinator watches the negotiation and retries
 * automatically, distinguishing two stall modes:
 *
 *  - **awaiting-answer** (no ANSWER arrived): a signaling-loss problem, so
 *    it re-sends the *same* cached OfferModel with a fresh `request_id`.
 *  - **checking** (ANSWER applied, ICE not connecting): an ICE problem, so
 *    it issues a single `createOffer({ iceRestart: true })`.
 *
 * Correctness invariants (see the design doc):
 *  - `offerGeneration` gates ANSWER / candidate / timer staleness across
 *    retries; `pcEpoch` (separate) invalidates callbacks of a replaced PC
 *    so a reused PC's genuine `connected` is never mistaken for stale.
 *  - The ANSWER watchdog is armed only once an OFFER genuinely reaches the
 *    wire (`markOfferSent`, driven by the signaling layer's `onSent`), so a
 *    queued-while-offline OFFER never burns a retry.
 *  - `retryInFlight` serializes concurrent triggers (an ICE `failed`
 *    callback racing a stall timer) into a single retry.
 */

/** What the negotiation should do in response to a trigger. Pure output of
 *  {@link decideIceAction}. */
export type IceDecision =
    | 'connected'
    | 'wait'
    | 'fail'
    | 'retry-answer'
    | 'retry-ice';

/** A reason the coordinator re-evaluates the negotiation. */
export type IceTrigger =
    | { kind: 'ice-state'; state: RTCIceConnectionState }
    | { kind: 'answer-timeout' }
    | { kind: 'ice-stall' };

/**
 * Pure decision: given a trigger and how many retries have already been
 * spent, what should happen? Extracted from the stateful coordinator so the
 * full matrix is unit-testable without timers or a PeerConnection.
 */
export function decideIceAction(
    trigger: IceTrigger,
    retryCount: number,
    maxRetry: number,
): IceDecision {
    switch (trigger.kind) {
        case 'ice-state':
            if (trigger.state === 'connected' || trigger.state === 'completed') {
                return 'connected';
            }
            // `failed` is terminal at the ICE layer — recover with an ICE
            // restart while budget remains. `checking` / `disconnected` /
            // `new` / `closed` heal on their own (or are handled by the
            // stall timers), so the negotiation just waits.
            if (trigger.state === 'failed') {
                return retryCount < maxRetry ? 'retry-ice' : 'fail';
            }
            return 'wait';
        case 'answer-timeout':
            return retryCount < maxRetry ? 'retry-answer' : 'fail';
        case 'ice-stall':
            return retryCount < maxRetry ? 'retry-ice' : 'fail';
    }
}

export type CandidateDisposition = 'apply' | 'queue' | 'reject';

/** How an OFFER is put on the wire. `onSent` MUST be invoked once the OFFER
 *  genuinely reaches the wire (not when merely queued). */
export type SendOfferFn = (
    requestId: string,
    onSent: (sentRequestId: string) => void,
) => void | Promise<void>;

export interface IceRetryConfig {
    answerTimeoutMs: number;
    /** Base `checking`-stall window for the first attempt. Kept short so a
     *  transient race recovers fast. */
    iceStallBaseMs: number;
    /** Cap for the stall window after exponential growth. */
    iceStallMaxMs: number;
    maxRetry: number;
}

/**
 * Stall window for a given attempt: capped exponential backoff
 * (`base * 2^attempt`, clamped to `max`). Early attempts re-roll quickly
 * (most failures are transient races); later attempts wait longer, which
 * both reduces churn (each retry re-gathers + re-mDNSes) and gives ICE's own
 * STUN retransmits more time to punch through a bad patch before a full
 * restart throws the in-progress checks away.
 */
export function iceStallWindowMs(
    attempt: number,
    baseMs: number,
    maxMs: number,
): number {
    return Math.min(baseMs * 2 ** attempt, maxMs);
}

export interface IceRetryDeps {
    /** Action A: re-send the cached immutable OfferModel with `requestId`. */
    resendCachedOffer: SendOfferFn;
    /** Action B: build a fresh `createOffer({iceRestart:true})`, apply it as
     *  local description, then send it with `requestId`. */
    sendIceRestartOffer: SendOfferFn;
    /** Negotiation reached `connected`/`completed`. */
    onConnected: () => void;
    /** Retries exhausted (or a send threw): surface a terminal failure. */
    onFailed: () => void;
    /** Mint a fresh wire `request_id`. */
    genRequestId: () => string;
    config: IceRetryConfig;
}

type Phase = 'idle' | 'awaiting-answer' | 'checking' | 'connected' | 'failed';

export interface IceRetryCoordinator {
    /** Epoch of the live PeerConnection. Callers capture it when wiring a
     *  PC's handlers and re-check it before acting, so a replaced PC's late
     *  callbacks are dropped. */
    currentEpoch(): number;
    /** Reset for a brand-new PeerConnection (initial connect): bumps epoch,
     *  clears timers, zeroes the retry budget. */
    resetForNewPc(): void;
    /** Allocate a `request_id` + generation for an OFFER about to be sent
     *  (initial connect). Send it with this id and wire `onSent` to
     *  {@link markOfferSent}. */
    beginOffer(): string;
    /** Signaling `onSent`: the OFFER reached the wire — arm the ANSWER
     *  watchdog for its generation. */
    markOfferSent(requestId: string): void;
    /** Whether an ANSWER belongs to the latest pending OFFER. */
    shouldAcceptAnswer(requestId: string): boolean;
    /** A matching ANSWER was applied: enter `checking`, record its remote
     *  ICE ufrag, arm the ICE-stall watchdog. */
    onAnswerApplied(remoteUfrag: string | null): void;
    /** Gate a trickled remote candidate by its `usernameFragment`. */
    classifyCandidate(
        usernameFragment: string | null | undefined,
    ): CandidateDisposition;
    /** PC ICE connection-state change (already epoch-gated by the caller). */
    onIceStateChange(state: RTCIceConnectionState): void;
    /** Teardown: bump epoch, clear timers, stop retrying. */
    dispose(): void;
    /** Test/diagnostic snapshot. */
    snapshot(): {
        phase: Phase;
        epoch: number;
        generation: number;
        pendingOfferRequestId: string | null;
        currentRemoteUfrag: string | null;
        retryCount: number;
    };
}

export function createIceRetryCoordinator(
    deps: IceRetryDeps,
): IceRetryCoordinator {
    const { config } = deps;

    let pcEpoch = 0;
    let offerGeneration = 0;
    let pendingOfferRequestId: string | null = null;
    let currentRemoteUfrag: string | null = null;
    let retryCount = 0;
    let retryInFlight = false;
    let phase: Phase = 'idle';

    let answerTimer: ReturnType<typeof setTimeout> | null = null;
    let iceStallTimer: ReturnType<typeof setTimeout> | null = null;

    const clearAnswerTimer = () => {
        if (answerTimer !== null) {
            clearTimeout(answerTimer);
            answerTimer = null;
        }
    };
    const clearIceStallTimer = () => {
        if (iceStallTimer !== null) {
            clearTimeout(iceStallTimer);
            iceStallTimer = null;
        }
    };
    const clearTimers = () => {
        clearAnswerTimer();
        clearIceStallTimer();
    };

    /** Allocate id + generation for a new OFFER and move to awaiting-answer. */
    const beginNegotiation = (): string => {
        offerGeneration += 1;
        pendingOfferRequestId = deps.genRequestId();
        currentRemoteUfrag = null;
        phase = 'awaiting-answer';
        clearTimers();
        return pendingOfferRequestId;
    };

    const failNow = () => {
        phase = 'failed';
        clearTimers();
        deps.onFailed();
    };

    const doRetry = async (kind: 'resend' | 'ice-restart') => {
        // Serialize concurrent triggers (e.g. ICE `failed` racing a stall
        // timer) into exactly one retry. The guard + the synchronous prefix
        // below (up to the first `await`) run before any re-entry, so the
        // send is dispatched exactly once.
        if (retryInFlight) {
            return;
        }
        retryInFlight = true;
        retryCount += 1;
        const requestId = beginNegotiation();
        const gen = offerGeneration;
        // Surfaced so a real-network repro can confirm auto-retry fired (and
        // attribute a recovery to it) without a debugger.
        console.info(
            `[ice-retry] attempt ${retryCount}/${config.maxRetry} via ${kind} (gen=${gen})`,
        );
        try {
            if (kind === 'resend') {
                await deps.resendCachedOffer(requestId, (sentId) =>
                    markOfferSent(sentId),
                );
            } else {
                await deps.sendIceRestartOffer(requestId, (sentId) =>
                    markOfferSent(sentId),
                );
            }
        } catch (e) {
            // A failed `createOffer` / `setLocalDescription` / send has no
            // ANSWER or timer to drive further recovery, so terminate
            // visibly (the user keeps the manual-retry affordance) — but only
            // if this attempt is still the current one.
            if (gen === offerGeneration) {
                console.warn('[ice-retry] offer send failed, giving up', e);
                failNow();
            }
        } finally {
            retryInFlight = false;
        }
    };

    const handleTrigger = (trigger: IceTrigger) => {
        const decision = decideIceAction(trigger, retryCount, config.maxRetry);
        switch (decision) {
            case 'connected':
                phase = 'connected';
                retryCount = 0;
                clearTimers();
                deps.onConnected();
                return;
            case 'wait':
                return;
            case 'fail':
                failNow();
                return;
            case 'retry-answer':
                void doRetry('resend');
                return;
            case 'retry-ice':
                void doRetry('ice-restart');
                return;
        }
    };

    const markOfferSent = (requestId: string) => {
        // Ignore the echo of a superseded OFFER, or one that already moved
        // past awaiting-answer.
        if (requestId !== pendingOfferRequestId || phase !== 'awaiting-answer') {
            return;
        }
        const gen = offerGeneration;
        clearAnswerTimer();
        answerTimer = setTimeout(() => {
            if (gen !== offerGeneration || phase !== 'awaiting-answer') {
                return;
            }
            handleTrigger({ kind: 'answer-timeout' });
        }, config.answerTimeoutMs);
    };

    return {
        currentEpoch: () => pcEpoch,

        resetForNewPc: () => {
            pcEpoch += 1;
            clearTimers();
            retryInFlight = false;
            retryCount = 0;
            offerGeneration = 0;
            pendingOfferRequestId = null;
            currentRemoteUfrag = null;
            phase = 'idle';
        },

        beginOffer: () => beginNegotiation(),

        markOfferSent,

        shouldAcceptAnswer: (requestId: string) =>
            requestId === pendingOfferRequestId,

        onAnswerApplied: (remoteUfrag: string | null) => {
            phase = 'checking';
            currentRemoteUfrag = remoteUfrag;
            clearAnswerTimer();
            const gen = offerGeneration;
            clearIceStallTimer();
            const stallMs = iceStallWindowMs(
                retryCount,
                config.iceStallBaseMs,
                config.iceStallMaxMs,
            );
            iceStallTimer = setTimeout(() => {
                if (gen !== offerGeneration || phase !== 'checking') {
                    return;
                }
                handleTrigger({ kind: 'ice-stall' });
            }, stallMs);
        },

        classifyCandidate: (usernameFragment) => {
            // Before an ANSWER is applied we don't yet know which ufrag is
            // current, so hold candidates for the post-ANSWER flush.
            if (phase === 'awaiting-answer' || currentRemoteUfrag === null) {
                return 'queue';
            }
            // A candidate without a ufrag predates per-generation tagging —
            // apply it to the current generation. Otherwise it must match.
            if (
                usernameFragment === null ||
                usernameFragment === undefined ||
                usernameFragment === currentRemoteUfrag
            ) {
                return 'apply';
            }
            return 'reject';
        },

        onIceStateChange: (state: RTCIceConnectionState) => {
            handleTrigger({ kind: 'ice-state', state });
        },

        dispose: () => {
            pcEpoch += 1;
            clearTimers();
            retryInFlight = false;
            phase = 'idle';
        },

        snapshot: () => ({
            phase,
            epoch: pcEpoch,
            generation: offerGeneration,
            pendingOfferRequestId,
            currentRemoteUfrag,
            retryCount,
        }),
    };
}
