import { useCallback, useEffect, useRef, useState } from 'react';
import { v4 } from 'uuid';
import {
    SIGNALING_TYPE_CODE_DIAGNOSE,
    SIGNALING_TYPE_CODE_DIAGNOSE_EVENT,
    SIGNALING_TYPE_CODE_DIAGNOSE_CANCEL,
    SIGNALING_TYPE_CODE_EXEC_CONTROL,
    SIGNALING_TYPE_CODE_EXEC_PREVIEW,
    SIGNALING_TYPE_CODE_EXEC_STATE_REPLY,
    SIGNALING_TYPE_CODE_RESOLVE_EXEC,
} from './constants';
import type { ExecPreview } from '../exec/use-confirm-exec';
import { deskErrorCodeEnum } from '@/services/types';
import type {
    ExecControlPayload,
    ExecStateReplyPayload,
} from '../exec/use-confirm-exec';
import type { SignalingMessage, SignalingSubscriber } from './use-desk-signaling';

export * from './diagnose-state';
import {
    INITIAL_STATE,
    SNAPSHOT_POLL_MS,
    buildSnapshotTranscript,
    extractStreamingSummary,
    snapshotConversationKey,
    snapshotLiveTurn,
    type DiagnoseEvent,
    type DiagnoseSessionSummary,
    type DiagnoseStartOptions,
    type DiagnoseState,
    type SessionSnapshot,
    type DiagnoseTimelineItem,
    type ToolActivity,
    type ToolActivityStatus,
} from './diagnose-state';

function upsertTool(
    timeline: DiagnoseTimelineItem[],
    activity: ToolActivity,
): DiagnoseTimelineItem[] {
    const existing = timeline.findIndex(
        (item) => item.kind === 'tool' && item.activity.callId === activity.callId,
    );
    const item: DiagnoseTimelineItem = {
        kind: 'tool',
        id: activity.callId,
        activity,
    };
    if (existing === -1) return [...timeline, item];
    return timeline.map((current, index) => (index === existing ? item : current));
}

function updateTool(
    timeline: DiagnoseTimelineItem[],
    callId: string,
    update: (activity: ToolActivity) => ToolActivity,
): DiagnoseTimelineItem[] {
    return timeline.map((item) =>
        item.kind === 'tool' && item.activity.callId === callId
            ? { ...item, activity: update(item.activity) }
            : item,
    );
}

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

// A cancel is acknowledged before the worker necessarily finishes reclaiming the
// process tree. The durable exec ledger therefore has to be queried until it
// settles; otherwise the UI can remain on "cancelling" until the much slower
// conversation snapshot poll happens to refresh it.
const CANCEL_STATE_POLL_MS = 500;

/**
 * Drives an AI diagnosis over signaling: sends a `Diagnose` request, aggregates
 * the notification-style `DiagnoseEvent` stream by `request_id` (ordered by
 * `seq`), and supports cancelling an in-flight run when the operator starts over.
 */
export function useDeskDiagnose({ deskId, subscribe, sendMessage }: UseDeskDiagnoseProps) {
    const [state, setState] = useState<DiagnoseState>(INITIAL_STATE);
    const [historySessions, setHistorySessions] = useState<DiagnoseSessionSummary[]>([]);
    const [historyLoading, setHistoryLoading] = useState(false);
    const [historyError, setHistoryError] = useState(false);
    const [canContinue, setCanContinue] = useState(true);
    const activeRequestRef = useRef<string | null>(null);
    // The conversation id threaded across follow-up turns. Minted lazily on the
    // first `start`; cleared on a desk change / reset so the next turn
    // opens a fresh conversation (subject-namespaced server-side).
    const conversationIdRef = useRef<string | null>(null);
    // Opaque selector returned by the history endpoint. It allows a legacy
    // session (whose client continuation id was never persisted) to be viewed
    // after selection while authorization remains actor/device scoped.
    const selectedSessionIdRef = useRef<string | null>(null);
    // Highest applied seq, so duplicate / out-of-order frames cannot corrupt
    // the accumulated summary. Reset to -1 per run (frames start at seq 0).
    const lastSeqRef = useRef<number>(-1);
    // Highest snapshot seq applied to the transcript, so a poll never regresses to
    // an older view of the shared session.
    const lastAppliedSeqRef = useRef<number>(-1);
    // Fetch the shared session snapshot and, when it advances and no live turn owns
    // the view, rebuild the transcript from it — rehydrating history and surfacing
    // an automation answer the request-scoped stream never delivered. Best-effort:
    // a network blip or a uniform not-accessible (a non-`SUCCESS` code) is left for a later
    // tick, when the device may have reconnected or the session may have appeared.
    const fetchSnapshot = useCallback(async () => {
        if (!deskId) return;
        const conversationId = conversationIdRef.current;
        const selectedSessionId = selectedSessionIdRef.current;
        if (!conversationId && !selectedSessionId) return;
        let res: Response;
        try {
            res = await fetch(
                `/api/my/diagnose-session?connection=${encodeURIComponent(deskId)}` +
                    (selectedSessionId
                        ? `&session=${encodeURIComponent(selectedSessionId)}`
                        : `&conversation=${encodeURIComponent(conversationId ?? '')}`),
                { credentials: 'include', headers: { Accept: 'application/json' } },
            );
        } catch {
            return; // transient: keep polling
        }
        if (!res.ok) return;
        let body: { success?: boolean; code?: number; data?: SessionSnapshot } | null = null;
        try {
            body = await res.json();
        } catch {
            return;
        }
        if (!body || body.success === false || body.code !== deskErrorCodeEnum.SUCCESS || !body.data) return;
        const snapshot = body.data;
        // Never regress to an older snapshot. An active snapshot is not a settled
        // transcript yet. While a live request owns the panel, only its matching
        // settled snapshot may recover the UI; this prevents a poll racing just
        // after `start()` from applying the prior turn's settled row.
        if (snapshot.seq <= lastAppliedSeqRef.current) return;
        if (snapshot.active) return;
        const activeRequest = activeRequestRef.current;
        if (activeRequest !== null && snapshot.requestId !== activeRequest) return;
        activeRequestRef.current = null;
        lastAppliedSeqRef.current = snapshot.seq;
        const history = buildSnapshotTranscript(snapshot.messages);
        // The snapshot is the whole settled transcript, so collapse any stale live
        // display into it. A non-empty transcript remains in the completed view,
        // where it is visible and the user can ask a follow-up.
        setState({
            ...INITIAL_STATE,
            phase: history.length > 0 ? 'done' : 'idle',
            conversationId,
            history,
            backgroundExecution: snapshot.activeExecutionGeneration
                ? {
                      executionGeneration: snapshot.activeExecutionGeneration,
                      cancelRequested: false,
                  }
                : null,
        });
    }, [deskId]);

    // A desk change rebinds the subject: restore that desk's persisted conversation
    // (so a reload / return continues it and rehydrates history from the shared
    // session) or start fresh when the desk has none.
    useEffect(() => {
        activeRequestRef.current = null;
        lastSeqRef.current = -1;
        lastAppliedSeqRef.current = -1;
        selectedSessionIdRef.current = null;
        setCanContinue(true);
        setHistorySessions([]);
        setHistoryError(false);
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
                conversationIdRef.current
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
            if (!deskId || !canContinue) return;
            // Reuse the conversation across follow-ups; mint one on the first turn.
            if (!conversationIdRef.current) {
                conversationIdRef.current = v4();
                selectedSessionIdRef.current = null;
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
        [canContinue, deskId, sendMessage],
    );

    // Full reset back to the question form. If a run is still in flight (the
    // user is starting over from `running`, e.g. a slow model or a dropped
    // connection left the panel spinning), notify the host so it can stop and
    // audit the abandoned run before we stop tracking the request.
    const reset = useCallback(() => {
        const requestId = activeRequestRef.current;
        if (deskId && requestId) {
            sendMessage(SIGNALING_TYPE_CODE_DIAGNOSE_CANCEL, null, deskId, requestId);
        }
        activeRequestRef.current = null;
        conversationIdRef.current = null;
        selectedSessionIdRef.current = null;
        lastSeqRef.current = -1;
        lastAppliedSeqRef.current = -1;
        setCanContinue(true);
        try {
            if (deskId) localStorage.removeItem(snapshotConversationKey(deskId));
        } catch {
            /* storage unavailable: nothing to clear */
        }
        setState(INITIAL_STATE);
    }, [deskId, sendMessage]);

    const refreshHistory = useCallback(async () => {
        if (!deskId) return;
        setHistoryLoading(true);
        setHistoryError(false);
        try {
            const res = await fetch(
                `/api/my/diagnose-sessions?connection=${encodeURIComponent(deskId)}&limit=50`,
                { credentials: 'include', headers: { Accept: 'application/json' } },
            );
            if (!res.ok) throw new Error('history request failed');
            const body = (await res.json()) as {
                success?: boolean;
                code?: number;
                data?: { sessions?: DiagnoseSessionSummary[] };
            };
            if (body.success === false || body.code !== deskErrorCodeEnum.SUCCESS || !body.data?.sessions) {
                throw new Error('history response failed');
            }
            setHistorySessions(body.data.sessions);
        } catch {
            setHistoryError(true);
        } finally {
            setHistoryLoading(false);
        }
    }, [deskId]);

    const restoreSession = useCallback(
        async (summary: DiagnoseSessionSummary) => {
            if (!deskId || summary.active) return;
            activeRequestRef.current = null;
            conversationIdRef.current = summary.conversationId ?? null;
            selectedSessionIdRef.current = summary.sessionId;
            lastSeqRef.current = -1;
            lastAppliedSeqRef.current = -1;
            setCanContinue(!!summary.conversationId);
            setState(INITIAL_STATE);
            try {
                if (summary.conversationId) {
                    localStorage.setItem(
                        snapshotConversationKey(deskId),
                        summary.conversationId,
                    );
                } else {
                    localStorage.removeItem(snapshotConversationKey(deskId));
                }
            } catch {
                // The selected history remains usable for this tab.
            }
            await fetchSnapshot();
        },
        [deskId, fetchSnapshot],
    );

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
        setState((prev) => {
            let awaiting = -1;
            prev.timeline.forEach((item, index) => {
                if (
                    item.kind === 'tool' &&
                    item.activity.status === 'awaiting_approval'
                ) {
                    awaiting = index;
                }
            });
            return {
                ...prev,
                pendingExec: null,
                timeline:
                    awaiting === -1
                        ? prev.timeline
                        : prev.timeline.map((item, index) =>
                              index === awaiting && item.kind === 'tool'
                                  ? {
                                        ...item,
                                        activity: {
                                            ...item.activity,
                                            status: 'running',
                                        },
                                    }
                                  : item,
                          ),
            };
        });
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

    /** Ask the host to stop the durable background command shown by the snapshot. */
    const cancelBackgroundExec = useCallback(() => {
        const generation = state.backgroundExecution?.executionGeneration;
        if (!deskId || !generation) return;
        const payload: ExecControlPayload = {
            execution_generation: generation,
            action: 'cancel',
            requested_by: 'diagnose-operator',
        };
        sendMessage(SIGNALING_TYPE_CODE_EXEC_CONTROL, payload, deskId);
        setState((prev) => ({
            ...prev,
            backgroundExecution: prev.backgroundExecution
                ? { ...prev.backgroundExecution, cancelRequested: true }
                : null,
        }));
    }, [deskId, sendMessage, state.backgroundExecution]);

    useEffect(() => {
        const background = state.backgroundExecution;
        if (!deskId || !background?.cancelRequested) return;

        const queryState = () => {
            const payload: ExecControlPayload = {
                execution_generation: background.executionGeneration,
                action: 'query_state',
            };
            sendMessage(SIGNALING_TYPE_CODE_EXEC_CONTROL, payload, deskId);
        };
        const interval = window.setInterval(queryState, CANCEL_STATE_POLL_MS);
        return () => window.clearInterval(interval);
    }, [
        deskId,
        sendMessage,
        state.backgroundExecution?.cancelRequested,
        state.backgroundExecution?.executionGeneration,
    ]);

    useEffect(() => {
        // Subscribe to the lossless signaling stream. DiagnoseEvent frames
        // are pushed rapidly (status / partial / final) and ordered by
        // `seq`; the previous single-value delivery could coalesce a burst
        // and drop intermediate frames, so streaming relies on this path.
        const handle = (message: SignalingMessage) => {
            if (message.signaling_type === SIGNALING_TYPE_CODE_EXEC_STATE_REPLY) {
                const payload = message.signaling_data as ExecStateReplyPayload | null;
                if (!payload) return;
                setState((prev) => {
                    if (
                        prev.backgroundExecution?.executionGeneration !==
                        payload.execution_generation
                    ) {
                        return prev;
                    }
                    return payload.state === 'running' || payload.state === 'reserved'
                        ? prev
                        : { ...prev, backgroundExecution: null };
                });
                return;
            }
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
                        const streamed = extractStreamingSummary(prev.partialSummary);
                        const activity: ToolActivity = {
                            callId: event.tool_call_id,
                            name: event.tool_name ?? event.tool_call_id,
                            status: event.awaiting_approval ? 'awaiting_approval' : 'running',
                            argumentsJson: event.tool_arguments_json ?? '',
                            output: null,
                            backgroundTaskId: null,
                        };
                        let timeline = prev.timeline;
                        if (streamed) {
                            timeline = [
                                ...timeline,
                                {
                                    kind: 'assistant',
                                    id: `assistant:${event.request_id}:${event.seq}`,
                                    text: streamed,
                                    provenance: null,
                                },
                            ];
                        }
                        return {
                            ...prev,
                            partialSummary: '',
                            timeline: upsertTool(timeline, activity),
                        };
                    }
                    case 'tool_finished': {
                        if (!event.tool_call_id) return prev;
                        const status: ToolActivityStatus = event.tool_ok ? 'ok' : 'failed';
                        return {
                            ...prev,
                            timeline: updateTool(
                                prev.timeline,
                                event.tool_call_id,
                                (activity) => ({
                                    ...activity,
                                    status,
                                    output: event.tool_output ?? '',
                                    backgroundTaskId:
                                        event.background_task_id ??
                                        activity.backgroundTaskId,
                                }),
                            ),
                        };
                    }
                    case 'answer':
                        activeRequestRef.current = null;
                        // A command may have crossed the foreground threshold just
                        // before this answer. Pull the settled snapshot immediately
                        // so its durable generation and cancel button appear without
                        // waiting for the periodic poll.
                        queueMicrotask(() => void fetchSnapshot());
                        {
                            const answer =
                                event.answer ??
                                extractStreamingSummary(prev.partialSummary);
                            const timeline = answer
                                ? [
                                      ...prev.timeline,
                                      {
                                          kind: 'assistant' as const,
                                          id: `assistant:${event.request_id}:${event.seq}`,
                                          text: answer,
                                          provenance: event.provenance ?? null,
                                      },
                                  ]
                                : prev.timeline;
                            return {
                                ...prev,
                                phase: 'done',
                                partialSummary: '',
                                timeline,
                                provenance: event.provenance ?? null,
                                pendingExec: null,
                            };
                        }
                    default:
                        return prev;
                }
            });
        };
        return subscribe(handle);
    }, [fetchSnapshot, subscribe]);

    return {
        state,
        start,
        reset,
        approveExec,
        rejectExec,
        cancelBackgroundExec,
        historySessions,
        historyLoading,
        historyError,
        refreshHistory,
        restoreSession,
        canContinue,
    };
}
