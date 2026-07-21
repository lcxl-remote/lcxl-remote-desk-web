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
import type { AiProvenance } from '@/components/ai-generated-mark';
import type { SignalingMessage, SignalingSubscriber } from './use-desk-signaling';

// Wire types — mirror `desk_agent_protocol::diagnose`. These ride the
// `Diagnose` / `DiagnoseEvent` / `DiagnoseCancel` signaling types as
// `signaling_data`; they are not part of the REST OpenAPI surface, so they are
// declared here like the other signaling payloads in this feature.

export type Confidence = 'high' | 'medium' | 'low';
export type RiskLevel = 'low' | 'medium' | 'high' | 'critical' | 'blocked';

export type Finding = {
    title: string;
    evidence_refs: string[];
    explanation: string;
};

export type SuggestedCommand = {
    shell: string;
    command: string;
    purpose: string;
    risk: RiskLevel;
    requires_confirmation: boolean;
};

export type Diagnosis = {
    summary: string;
    confidence: Confidence;
    findings: Finding[];
    commands: SuggestedCommand[];
    next_steps: string[];
    missing_info: string[];
    collected: string[];
};

// `status` / `partial` / `final` / `error` are the single-turn diagnose frames.
// `turn_started` / `tool_started` / `tool_finished` / `answer` are the agentic
// multi-turn loop's frames: a turn boundary, a tool call's start (a read tool, or
// a mutating tool awaiting approval) and finish, and a terminal free-text answer
// (distinct from `final`, which carries a structured `Diagnosis`).
export type DiagnoseEventKind =
    | 'status'
    | 'partial'
    | 'final'
    | 'error'
    | 'turn_started'
    | 'tool_started'
    | 'tool_finished'
    | 'answer';

export type AgentError = {
    kind: string;
    message: string;
    retryable: boolean;
    safe_for_model: boolean;
    /** Optional business code (a `DeskErrorCode`) the control end localizes. */
    error_code?: number | null;
};

export type DiagnoseEvent = {
    request_id: string;
    seq: number;
    kind: DiagnoseEventKind;
    status?: string | null;
    partial_summary?: string | null;
    final_result?: Diagnosis | null;
    error?: AgentError | null;
    /** `turn_started`: the id of the agentic turn that started. */
    turn_id?: string | null;
    /** `tool_started`: the model-facing tool name. */
    tool_name?: string | null;
    /** `tool_started` / `tool_finished`: the tool call id. */
    tool_call_id?: string | null;
    /** `tool_started`: a mutating tool waiting for the operator's approval. */
    awaiting_approval?: boolean;
    /** `tool_finished`: whether the call produced a usable result. */
    tool_ok?: boolean | null;
    /** `answer`: the agentic turn's final natural-language answer. */
    answer?: string | null;
    /** `final` / `answer`: machine-readable AI marking for the content frame. */
    provenance?: AiProvenance | null;
};

/** A tool call's lifecycle status, shown in the agentic activity timeline. */
export type ToolActivityStatus = 'running' | 'awaiting_approval' | 'ok' | 'failed';

/** One tool call's visible activity for the current run (keyed by call id). */
export type ToolActivity = {
    callId: string;
    name: string;
    status: ToolActivityStatus;
};

export type DiagnoseStartOptions = {
    includeScreen?: boolean;
    contextKinds?: string[];
    /** BCP-47 tag of the current UI language, so the AI answers in it. */
    locale?: string;
    /** Manager-only user-selected agent model. Omitted (null/undefined) when the
     *  model selector is hidden (open-source signal); the server then resolves the
     *  default, keeping the flow identical across both signaling targets. */
    modelId?: number | null;
    /** Manager-only active-organization hint. Set only in the console's org view;
     *  omitted (undefined) by the personal view and the open-source control end, so
     *  no `org_id` rides the wire and the request resolves against the personal
     *  subject exactly as before. The manager validates it and silently degrades to
     *  personal if it fails, so forwarding it is always safe. */
    orgId?: number;
};

/**
 * Extract a human-readable streaming summary from a partially-received model
 * response so the panel can show flowing text instead of a growing raw JSON
 * string (or a model's raw reasoning) while the structured output is still
 * being produced.
 *
 * Mirrors the backend parser's tolerance (`desk-diagnose-core`): a reasoning
 * model (e.g. DeepSeek-R1) prepends a `<think>...</think>` block, and some
 * models wrap the JSON in a ```json fence or a sentence of prose. Those would
 * otherwise stream out as raw, unformatted text. The logic is:
 *
 * 1. Drop completed `<think>...</think>` blocks; if a block is still open (its
 *    closing tag has not streamed yet) the whole tail is reasoning, so nothing
 *    is shown yet.
 * 2. From the first `{` (skipping any fence / prose preamble) read the value of
 *    the `"summary"` string field as it grows — tolerant of the document being
 *    truncated mid-string and of a trailing incomplete escape — and return it
 *    decoded. Before `"summary"` appears, return an empty string so the caller
 *    falls back to a "working" indicator.
 * 3. With no `{` at all (free-text mode), return the prose as-is.
 */
export function extractStreamingSummary(raw: string): string {
    if (!raw) return '';

    // Step 1: strip reasoning. Remove completed think blocks; truncate at an
    // unterminated one (everything after it is still reasoning).
    let text = raw.replace(/<think>[\s\S]*?<\/think>/g, '');
    const openThink = text.lastIndexOf('<think>');
    if (openThink !== -1) text = text.slice(0, openThink);

    // Step 3 (no JSON yet): free-text prose, shown directly.
    const brace = text.indexOf('{');
    if (brace === -1) return text.trimStart();

    // Step 2: read the "summary" value from the first JSON object, ignoring any
    // fence / prose before the opening brace.
    const json = text.slice(brace);
    const key = json.match(/"summary"\s*:\s*"/);
    if (!key || key.index === undefined) return '';

    let out = '';
    for (let i = key.index + key[0].length; i < json.length; i++) {
        const ch = json[i];
        if (ch === '"') break; // closing quote of the summary value
        if (ch !== '\\') {
            out += ch;
            continue;
        }
        // Escape sequence; bail out if it is truncated at the end of the stream.
        const next = json[i + 1];
        if (next === undefined) break;
        switch (next) {
            case 'n': out += '\n'; break;
            case 't': out += '\t'; break;
            case 'r': out += '\r'; break;
            case 'b': out += '\b'; break;
            case 'f': out += '\f'; break;
            case '"': out += '"'; break;
            case '\\': out += '\\'; break;
            case '/': out += '/'; break;
            case 'u': {
                const hex = json.slice(i + 2, i + 6);
                if (hex.length < 4) return out; // incomplete \uXXXX at stream end
                out += String.fromCharCode(parseInt(hex, 16));
                i += 4;
                break;
            }
            default: out += next;
        }
        i += 1; // consume the escaped character
    }
    return out;
}

// `idle` before a run, `running` while frames stream, `done` on a terminal
// `final` (or after a handoff that keeps the gathered result), `error` on a
// terminal `error` frame.
export type DiagnosePhase = 'idle' | 'running' | 'done' | 'error';

/**
 * One settled turn of the conversation, frozen for the transcript once a newer
 * follow-up question starts. The live (current) turn is held in the top-level
 * state fields; when the next `start` begins, the settled live turn is snapshot
 * into `history` so the panel can render the running conversation.
 */
export type DiagnoseHistoryTurn = {
    requestId: string;
    /** The question the user asked for this turn. */
    question: string;
    /** Structured result, if a `final` frame arrived (single-turn path). */
    result: Diagnosis | null;
    /** Agentic free-text answer, if an `answer` frame arrived. */
    answer: string | null;
    /** Streaming summary captured for this turn (fallback display text). */
    summary: string;
    /** The turn's tool activity. */
    tools: ToolActivity[];
    /** How the turn settled. */
    phase: 'done' | 'error';
    /** Failure message if the turn errored. */
    error: string | null;
    /**
     * Machine-readable AI marking (Art.50(2)) captured for this settled turn,
     * so the transcript keeps marking past AI answers the same way the live
     * turn does. Null does not mean "not AI" — an AI reply being present marks
     * the turn (fail-closed); this only carries model / timestamp when known.
     */
    provenance: AiProvenance | null;
};

export type DiagnoseState = {
    phase: DiagnosePhase;
    /**
     * Stable id threaded across follow-up turns so the backend continues the
     * same agentic session (the model sees prior turns). Minted on the first
     * `start`, regenerated on a desk change / `reset` / `handoff`.
     */
    conversationId: string | null;
    requestId: string | null;
    /** The current (live) turn's question. */
    question: string;
    /** Latest lifecycle phase name (collecting / redacting / modeling). */
    status: string | null;
    /** Accumulated streaming summary fragments. */
    partialSummary: string;
    /** The structured result, set on a `final` frame (single-turn path). */
    result: Diagnosis | null;
    /** A human-readable failure message, set on an `error` frame. */
    error: string | null;
    /** Optional business code from the error frame, localized on display. */
    errorCode: number | null;
    /** Latest agentic turn id (set on a `turn_started` frame). */
    turnId: string | null;
    /** The agentic tool-activity timeline, in call order (agentic path). */
    tools: ToolActivity[];
    /** The agentic turn's final answer text, set on an `answer` frame. */
    answer: string | null;
    /**
     * Machine-readable AI marking for the current result / answer (Art.50(2)),
     * set on a `final` / `answer` frame. Null does not mean "not AI" — the
     * result / answer being present already marks the content AI (fail-closed);
     * this only carries the model / timestamp metadata when known.
     */
    provenance: AiProvenance | null;
    /**
     * A mutating command the agentic loop initiated and is now blocked on,
     * awaiting the operator's approval. Set from the unsolicited `ExecPreview`
     * the backend pushes while the loop is parked; cleared once the operator
     * resolves it or the run ends. At most one is pending at a time because the
     * loop executes tools sequentially.
     */
    pendingExec: ExecPreview | null;
    /** Prior settled turns of this conversation, oldest first. */
    history: DiagnoseHistoryTurn[];
};

const INITIAL_STATE: DiagnoseState = {
    phase: 'idle',
    conversationId: null,
    requestId: null,
    question: '',
    status: null,
    partialSummary: '',
    result: null,
    error: null,
    errorCode: null,
    turnId: null,
    tools: [],
    answer: null,
    provenance: null,
    pendingExec: null,
    history: [],
};

/**
 * Freeze the previous live turn into a transcript entry when a follow-up turn
 * begins. Only a settled (`done` / `error`) turn is captured; starting the very
 * first turn from `idle` adds nothing.
 */
function snapshotLiveTurn(prev: DiagnoseState): DiagnoseHistoryTurn[] {
    if ((prev.phase !== 'done' && prev.phase !== 'error') || !prev.requestId) {
        return prev.history;
    }
    return [
        ...prev.history,
        {
            requestId: prev.requestId,
            question: prev.question,
            result: prev.result,
            answer: prev.answer,
            summary: extractStreamingSummary(prev.partialSummary),
            tools: prev.tools,
            phase: prev.phase,
            error: prev.error,
            provenance: prev.provenance,
        },
    ];
}

// --- Session snapshot (the "tmux view") -------------------------------------
//
// The full conversation is authoritative in the manager's shared `agent_session`
// row. The panel fetches this snapshot to rehydrate its transcript, which keeps
// history across a reload and — the reason it exists — surfaces an automation
// turn's answer (fired server-side after a background command completes) that the
// request-scoped live stream never delivers. The open-source signal server has no
// such endpoint; a 404 disables the feature so the flow stays identical there.

/** One message in a diagnose-session snapshot (mirrors the manager REST DTO). */
export type SnapshotMessage = {
    id: string;
    role: 'user' | 'assistant' | 'tool' | 'system_event' | 'untrusted_output' | 'system';
    text: string;
    toolCalls?: { id: string; name: string; argumentsJson: string }[];
    toolCallId?: string | null;
};

/** A settled snapshot: the persisted transcript plus a monotonic sequence. */
export type SessionSnapshot = {
    seq: number;
    messages: SnapshotMessage[];
};

/** Poll cadence for the snapshot while the tab is visible (a staleness floor; a
 *  server push would only make it faster). */
export const SNAPSHOT_POLL_MS = 20000;

/** localStorage key persisting a desk's conversation intent, so a reload rejoins
 *  the same server-side session instead of opening a fresh one. */
export function snapshotConversationKey(deskId: string): string {
    return `lrd:diagnose-conv:${deskId}`;
}

/**
 * Rebuild the settled transcript from a snapshot: group the flat message list into
 * turns at each `user` message. Assistant text becomes the turn's answer (several
 * assistant answers in one turn — e.g. an automation follow-up appended after the
 * original — are joined), and assistant tool calls become the turn's tool activity.
 * Internal `tool` / `system_event` / `untrusted_output` messages carry no
 * user-facing turn text and are skipped. Pure, so it is unit-tested directly.
 */
export function buildSnapshotTranscript(messages: SnapshotMessage[]): DiagnoseHistoryTurn[] {
    const turns: DiagnoseHistoryTurn[] = [];
    const open = (id: string, question: string): DiagnoseHistoryTurn => ({
        requestId: id,
        question,
        result: null,
        answer: null,
        summary: '',
        tools: [],
        phase: 'done',
        error: null,
        provenance: null,
    });
    let current: DiagnoseHistoryTurn | null = null;
    for (const m of messages) {
        if (m.role === 'user') {
            if (current) turns.push(current);
            current = open(m.id, m.text);
        } else if (m.role === 'assistant') {
            if (!current) current = open(m.id, '');
            if (m.text) {
                current.answer = current.answer ? `${current.answer}\n\n${m.text}` : m.text;
            }
            for (const tc of m.toolCalls ?? []) {
                current.tools.push({ callId: tc.id, name: tc.name, status: 'ok' });
            }
        }
        // `tool` / `system_event` / `untrusted_output`: internal, no visible turn text.
    }
    if (current) turns.push(current);
    return turns;
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
                                tt.callId === event.tool_call_id ? { ...tt, status } : tt,
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
