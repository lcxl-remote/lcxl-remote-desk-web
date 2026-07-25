import { useCallback, useEffect, useRef, useState } from 'react';
import { v4 } from 'uuid';
import {
    SIGNALING_TYPE_CODE_DIAGNOSE,
    SIGNALING_TYPE_CODE_DIAGNOSE_EVENT,
    SIGNALING_TYPE_CODE_DIAGNOSE_CANCEL,
    SIGNALING_TYPE_CODE_EXEC_PREVIEW,
    SIGNALING_TYPE_CODE_RESOLVE_EXEC,
} from './constants';
import type { ExecPreview } from '../exec/use-confirm-exec';
import type { SignalingMessage, SignalingSubscriber } from './use-desk-signaling';

export * from './diagnose-state';
import {
    INITIAL_STATE,
    SNAPSHOT_POLL_MS,
    buildSnapshotTranscript,
    snapshotConversationKey,
    snapshotLiveTurn,
    type DiagnoseEvent,
    type DiagnoseStartOptions,
    type DiagnoseState,
    type SessionSnapshot,
    type ToolActivity,
    type ToolActivityStatus,
} from './diagnose-state';

type UseDeskDiagnoseProps = {
    deskId: string | null;
    subscribe: (handler: SignalingSubscriber) => () => void;
    sendMessage: (
        type: number,
        data: unknown,
        connectionId?: string,
        requestId?: string,
    ) => string;
};

/**
 * Drives an AI diagnosis over signaling: sends a `Diagnose` request, aggregates
 * the notification-style `DiagnoseEvent` stream by `request_id` (ordered by
 * `seq`), and exposes a 转人工 (handoff) action that closes the flow while
 * retaining the gathered result and notifies the host for auditing.
 */
export function useDeskDiagnose({ deskId, subscribe, sendMessage }: UseDeskDiagnoseProps) {
    const [state, setState] = useState<DiagnoseState>(INITIAL_STATE);
    const activeRequestRef = useRef<string | null>(null);
    // The conversation id threaded across follow-up turns. Minted lazily on the
    // first `start`; cleared on a desk change / reset / handoff so the next turn
    // opens a fresh conversation (subject-namespaced server-side).
    const conversationIdRef = useRef<string | null>(null);
    // Highest applied seq, so duplicate / out-of-order frames cannot corrupt
    // the accumulated summary. Reset to -1 per run (frames start at seq 0).
    const lastSeqRef = useRef<number>(-1);
    // Highest snapshot seq applied to the transcript, so a poll never regresses to
    // an older view of the shared session.
    const lastAppliedSeqRef = useRef<number>(-1);
    // Set once the snapshot endpoint is found absent (open-source signal → 404), so
    // the panel stops polling and behaves exactly as it did before this feature.
    const snapshotUnsupportedRef = useRef(false);

    // Fetch the shared session snapshot and, when it advances and no live turn owns
    // the view, rebuild the transcript from it — rehydrating history and surfacing
    // an automation answer the request-scoped stream never delivered. Best-effort: a
    // network blip retries next tick; a 404 disables the feature (open-source
    // signal); a uniform not-accessible (`code !== 0`) is left for a later tick, when
    // the device may have reconnected or the session may have appeared.
    const fetchSnapshot = useCallback(async () => {
        if (!deskId || snapshotUnsupportedRef.current) return;
        const conversationId = conversationIdRef.current;
        if (!conversationId) return;
        let res: Response;
        try {
            res = await fetch(
                `/api/my/diagnose-session?connection=${encodeURIComponent(deskId)}` +
                    `&conversation=${encodeURIComponent(conversationId)}`,
                { credentials: 'include', headers: { Accept: 'application/json' } },
            );
        } catch {
            return; // transient: keep polling
        }
        if (res.status === 404) {
            snapshotUnsupportedRef.current = true; // no such endpoint (open-source signal)
            return;
        }
        if (!res.ok) return;
        let body: { success?: boolean; code?: number; data?: SessionSnapshot } | null = null;
        try {
            body = await res.json();
        } catch {
            return;
        }
        if (!body || body.success === false || body.code !== 0 || !body.data) return;
        const snapshot = body.data;
        // Never regress to an older snapshot; never overwrite a live turn (reconcile
        // once it settles and the next poll advances).
        if (snapshot.seq <= lastAppliedSeqRef.current) return;
        if (activeRequestRef.current !== null) return;
        lastAppliedSeqRef.current = snapshot.seq;
        const history = buildSnapshotTranscript(snapshot.messages);
        // The snapshot is the whole settled transcript, so collapse any settled
        // current-turn display into it and return to idle (no duplicated turn).
        setState({ ...INITIAL_STATE, conversationId, history });
    }, [deskId]);

    // A desk change rebinds the subject: restore that desk's persisted conversation
    // (so a reload / return continues it and rehydrates history from the shared
    // session) or start fresh when the desk has none.
    useEffect(() => {
        activeRequestRef.current = null;
        lastSeqRef.current = -1;
        lastAppliedSeqRef.current = -1;
        let restored: string | null = null;
        try {
            restored = deskId ? localStorage.getItem(snapshotConversationKey(deskId)) : null;
        } catch {
            restored = null;
        }
        conversationIdRef.current = restored;
        setState(INITIAL_STATE);
        if (deskId && restored) void fetchSnapshot();
    }, [deskId, fetchSnapshot]);

    // Poll the snapshot while the tab is visible (a staleness floor), and fetch
    // promptly on regaining visibility; paused when hidden, disabled once absent.
    useEffect(() => {
        if (!deskId) return;
        const tick = () => {
            if (
                document.visibilityState === 'visible' &&
                conversationIdRef.current &&
                !snapshotUnsupportedRef.current
            ) {
                void fetchSnapshot();
            }
        };
        const interval = window.setInterval(tick, SNAPSHOT_POLL_MS);
        const onVisibility = () => {
            if (document.visibilityState === 'visible') tick();
        };
        document.addEventListener('visibilitychange', onVisibility);
        return () => {
            window.clearInterval(interval);
            document.removeEventListener('visibilitychange', onVisibility);
        };
    }, [deskId, fetchSnapshot]);

    const start = useCallback(
        (question: string, options?: DiagnoseStartOptions) => {
            if (!deskId) return;
            // Reuse the conversation across follow-ups; mint one on the first turn.
            if (!conversationIdRef.current) {
                conversationIdRef.current = v4();
                lastAppliedSeqRef.current = -1;
            }
            const conversationId = conversationIdRef.current;
            // Persist it so a reload rejoins this same server-side session and can
            // rehydrate its transcript (including any automation follow-up).
            try {
                localStorage.setItem(snapshotConversationKey(deskId), conversationId);
            } catch {
                // Storage unavailable (private mode): the session still works; it
                // just will not rehydrate after a reload.
            }
            const data = {
                question,
                include_screen: options?.includeScreen ?? false,
                context_kinds: options?.contextKinds ?? [],
                locale: options?.locale,
                conversation_id: conversationId,
                // Absent (undefined) unless the manager model selector supplied a
                // choice; the open-source server ignores the field anyway.
                model_id: options?.modelId ?? undefined,
                // Absent (undefined) unless the console's org view supplied it; a
                // non-authoritative hint the manager validates, ignored open-source.
                org_id: options?.orgId ?? undefined,
            };
            const requestId = sendMessage(SIGNALING_TYPE_CODE_DIAGNOSE, data, deskId);
            activeRequestRef.current = requestId;
            lastSeqRef.current = -1;
            setState((prev) => ({
                ...INITIAL_STATE,
                phase: 'running',
                conversationId,
                requestId,
                question,
                // Freeze the prior settled turn into the transcript.
                history: snapshotLiveTurn(prev),
            }));
        },
        [deskId, sendMessage],
    );

    // 转人工: stop tracking the stream, keep whatever evidence / suggestions
    // were gathered visible in the panel, and notify the host (which records an
    // `ai.task.cancelled` audit). Correlated by the original diagnosis id.
    const handoff = useCallback(() => {
        const requestId = activeRequestRef.current;
        if (deskId && requestId) {
            sendMessage(SIGNALING_TYPE_CODE_DIAGNOSE_CANCEL, null, deskId, requestId);
        }
        activeRequestRef.current = null;
        // A handed-off turn leaves an orphaned session behind; any follow-up must
        // open a new conversation rather than re-claim it — so drop the persisted
        // intent too, and stop applying its snapshot.
        conversationIdRef.current = null;
        lastAppliedSeqRef.current = -1;
        try {
            if (deskId) localStorage.removeItem(snapshotConversationKey(deskId));
        } catch {
            /* storage unavailable: nothing to clear */
        }
        setState((prev) => ({ ...prev, phase: 'done', pendingExec: null }));
    }, [deskId, sendMessage]);

    // Full reset back to the question form. If a run is still in flight (the
    // user is starting over from `running`, e.g. a slow model or a dropped
    // connection left the panel spinning), notify the host so it can audit the
    // abandonment — mirroring 转人工 — before we stop tracking the request.
    const reset = useCallback(() => {
        const requestId = activeRequestRef.current;
        if (deskId && requestId) {
            sendMessage(SIGNALING_TYPE_CODE_DIAGNOSE_CANCEL, null, deskId, requestId);
        }
        activeRequestRef.current = null;
        conversationIdRef.current = null;
        lastSeqRef.current = -1;
        lastAppliedSeqRef.current = -1;
        try {
            if (deskId) localStorage.removeItem(snapshotConversationKey(deskId));
        } catch {
            /* storage unavailable: nothing to clear */
        }
        setState(INITIAL_STATE);
    }, [deskId, sendMessage]);

    // Approve the command the agentic loop is parked on: send `ResolveExec`
    // (correlated by the server-minted `exec_request_id`) so the backend
    // unblocks the loop and dispatches the command to the worker. The result
    // flows back through the loop and surfaces as the tool-timeline entry's
    // completion, not a separate `ExecResult` frame.
    const approveExec = useCallback(() => {
        const reqId = state.pendingExec?.exec_request_id;
        if (!deskId || !reqId) return;
        sendMessage(
            SIGNALING_TYPE_CODE_RESOLVE_EXEC,
            { exec_request_id: reqId, decision: 'approve' },
            deskId,
        );
        setState((prev) => ({ ...prev, pendingExec: null }));
    }, [deskId, sendMessage, state.pendingExec]);

    // Reject the parked command: send `ResolveExec` with `reject` so the loop
    // gets a rejection outcome and can adapt, then clear the approval card.
    const rejectExec = useCallback(() => {
        const reqId = state.pendingExec?.exec_request_id;
        if (deskId && reqId) {
            sendMessage(
                SIGNALING_TYPE_CODE_RESOLVE_EXEC,
                { exec_request_id: reqId, decision: 'reject' },
                deskId,
            );
        }
        setState((prev) => ({ ...prev, pendingExec: null }));
    }, [deskId, sendMessage, state.pendingExec]);

    useEffect(() => {
        // Subscribe to the lossless signaling stream. DiagnoseEvent frames
        // are pushed rapidly (status / partial / final) and ordered by
        // `seq`; the previous single-value delivery could coalesce a burst
        // and drop intermediate frames, so streaming relies on this path.
        const handle = (message: SignalingMessage) => {
            // An unsolicited `ExecPreview` arriving while a run is in flight is the
            // agentic loop asking to run a command. The suggested-command flow
            // (`use-desk-exec`) owns previews it requested and correlates them by its
            // own ConfirmExec request_id; it drops anything it did not request, so
            // claiming the agentic one here causes no double handling.
            if (message.signaling_type === SIGNALING_TYPE_CODE_EXEC_PREVIEW) {
                if (activeRequestRef.current === null) return;
                const preview = message.signaling_data as ExecPreview | null;
                if (!preview || !preview.exec_request_id) return;
                setState((prev) =>
                    prev.phase === 'running' ? { ...prev, pendingExec: preview } : prev,
                );
                return;
            }

            if (message.signaling_type !== SIGNALING_TYPE_CODE_DIAGNOSE_EVENT) return;
            const event = message.signaling_data as DiagnoseEvent | null;
            if (!event || event.request_id !== activeRequestRef.current) return;
            // Ignore stale / replayed frames.
            if (event.seq <= lastSeqRef.current) return;
            lastSeqRef.current = event.seq;

            setState((prev) => {
                switch (event.kind) {
                    case 'status':
                        return { ...prev, status: event.status ?? prev.status };
                    case 'partial':
                        return {
                            ...prev,
                            partialSummary: prev.partialSummary + (event.partial_summary ?? ''),
                        };
                    case 'final':
                        activeRequestRef.current = null;
                        return {
                            ...prev,
                            phase: 'done',
                            result: event.final_result ?? null,
                            provenance: event.provenance ?? null,
                            pendingExec: null,
                        };
                    case 'error':
                        activeRequestRef.current = null;
                        return {
                            ...prev,
                            phase: 'error',
                            error: event.error?.message ?? 'diagnosis failed',
                            errorCode: event.error?.error_code ?? null,
                            pendingExec: null,
                        };
                    case 'turn_started':
                        return { ...prev, turnId: event.turn_id ?? prev.turnId };
                    case 'tool_started': {
                        if (!event.tool_call_id) return prev;
                        const activity: ToolActivity = {
                            callId: event.tool_call_id,
                            name: event.tool_name ?? event.tool_call_id,
                            status: event.awaiting_approval ? 'awaiting_approval' : 'running',
                            argumentsJson: event.tool_arguments_json ?? '',
                            output: null,
                        };
                        // Replace an existing entry for the same call (e.g. a re-emit)
                        // rather than duplicating it.
                        const tools = prev.tools.some((tt) => tt.callId === activity.callId)
                            ? prev.tools.map((tt) =>
                                  tt.callId === activity.callId ? activity : tt,
                              )
                            : [...prev.tools, activity];
                        return { ...prev, tools };
                    }
                    case 'tool_finished': {
                        if (!event.tool_call_id) return prev;
                        const status: ToolActivityStatus = event.tool_ok ? 'ok' : 'failed';
                        return {
                            ...prev,
                            tools: prev.tools.map((tt) =>
                                tt.callId === event.tool_call_id
                                    ? {
                                          ...tt,
                                          status,
                                          output: event.tool_output ?? '',
                                      }
                                    : tt,
                            ),
                        };
                    }
                    case 'answer':
                        activeRequestRef.current = null;
                        return {
                            ...prev,
                            phase: 'done',
                            answer: event.answer ?? '',
                            provenance: event.provenance ?? null,
                            pendingExec: null,
                        };
                    default:
                        return prev;
                }
            });
        };
        return subscribe(handle);
    }, [subscribe]);

    return { state, start, handoff, reset, approveExec, rejectExec };
}
