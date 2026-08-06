import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { v4 } from 'uuid';
import {
    SIGNALING_TYPE_CODE_TERMINAL_COPILOT_ASK,
    SIGNALING_TYPE_CODE_TERMINAL_COPILOT_EVENT,
    SIGNALING_TYPE_CODE_TERMINAL_COPILOT_CANCEL,
} from '../desk/constants';
import type { AiProvenance } from '@/components/ai-generated-mark';
import type { SignalingMessage, SignalingSubscriber } from '../desk/use-desk-signaling';

// Wire types — mirror `desk_agent_protocol::terminal_copilot`. They ride the
// `TerminalCopilot{Ask,Event,Cancel}` signaling types as `signaling_data`; like
// the diagnose payloads they are not part of the REST OpenAPI surface, so they
// are declared here.

export type TerminalCopilotMode = 'how_to' | 'explain_error';

export type RiskLevel = 'low' | 'medium' | 'high' | 'critical' | 'blocked';

/** Server-computed execution decision. Drives the available actions — the
 *  control end never trusts a model-self-reported field. */
export type ExecDecision = 'confirm_required' | 'not_executable' | 'blocked';

export type CommandSuggestion = {
    command: string;
    shell: string;
    cwd?: string | null;
    note: string;
    risk: RiskLevel;
    decision: ExecDecision;
};

export type TerminalCopilotAnswer = {
    explanation_md: string;
    suggestions: CommandSuggestion[];
};

/** One prior exchange replayed to the server so a stateless central brain (the
 *  OSS signal path) can continue a multi-turn conversation. The manager path
 *  ignores it (its DB session is authoritative). */
export type CopilotHistoryTurn = {
    user: string;
    assistant: string;
};

export type TerminalCopilotEventKind =
    | 'partial'
    | 'partial_committed'
    | 'retracted'
    | 'tool_started'
    | 'final'
    | 'error';

export type AgentError = {
    kind: string;
    message: string;
    retryable: boolean;
    safe_for_model: boolean;
    /** Optional business code (a `DeskErrorCode`) the control end localizes. */
    error_code?: number | null;
};
export type StreamRetractionReason =
    | 'policy_blocked'
    | 'safe_redirect'
    | 'safety_unavailable'
    | 'incomplete';


export type TerminalCopilotEvent = {
    request_id: string;
    seq: number;
    kind: TerminalCopilotEventKind;
    partial_text?: string | null;
    tool_name?: string | null;
    answer?: TerminalCopilotAnswer | null;
    error?: AgentError | null;
    /** `retracted`: fixed local policy/unavailable message selector. */
    retraction_reason?: StreamRetractionReason | null;
    /** `final`: machine-readable AI marking for the answer content frame. */
    provenance?: AiProvenance | null;
};

/** Non-authoritative terminal context — a prompt hint only. The server
 *  re-redacts and length-caps it before any model dial. */
export type TerminalContext = {
    os: string;
    shell: string;
    cwd?: string | null;
    recent_output: string;
    last_command?: string | null;
    error_text?: string | null;
};

/** One read-only evidence tool the copilot dispatched, shown in the activity
 *  line. The copilot has no approval-gated tools, so there is no awaiting state. */
export type CopilotTool = {
    name: string;
};

export type CopilotPhase = 'idle' | 'running' | 'done' | 'error';

/** One turn of the visible conversation: the operator's message and the
 *  assistant's answer (null while the turn is still in flight). */
export type TerminalCopilotTurn = {
    /** The operator's message — their `how_to` request, or the error passage for
     *  `explain_error`. Shown as the user bubble and replayed as history. */
    question: string;
    mode: TerminalCopilotMode;
    /** The structured answer, set on the turn's terminal `final` frame. */
    answer: TerminalCopilotAnswer | null;
    /**
     * Machine-readable AI marking for the answer (Art.50(2)), set on the `final`
     * frame. Null does not mean "not AI" — an answer being present already marks
     * the content AI (fail-closed); this only carries model / timestamp metadata.
     */
    provenance: AiProvenance | null;
};

export type CopilotState = {
    phase: CopilotPhase;
    requestId: string | null;
    /** Stable id threaded across follow-up asks so the backend continues one
     *  conversation (the manager keys its DB session on it; the signal path is
     *  stateless and relies on the replayed `history`). */
    conversationId: string | null;
    /** The mode of the current/last ask. */
    mode: TerminalCopilotMode;
    /** The full conversation, oldest first. The last entry is the in-flight turn
     *  while `phase === 'running'`. */
    turns: TerminalCopilotTurn[];
    /** Accumulated streaming explanation fragments for the in-flight turn
     *  (provisional; the Final answer's `explanation_md` is authoritative). */
    partialText: string;
    /** Reviewed prose committed by prior tool-calling steps in this turn. */
    committedText: string;
    /** Read-only tools dispatched this run, in order. */
    tools: CopilotTool[];
    /** A human-readable failure message, set on the terminal `error` frame. */
    error: string | null;
    /** Optional business code from the error frame, localized by the panel. */
    errorCode: number | null;
    /** Closed reason for a server-requested provisional-text retraction. */
    retractionReason?: StreamRetractionReason | null;
};

const INITIAL_STATE: CopilotState = {
    phase: 'idle',
    requestId: null,
    retractionReason: null,
    conversationId: null,
    mode: 'how_to',
    turns: [],
    partialText: '',
    tools: [],
    committedText: '',
    error: null,
    errorCode: null,
};

/** Client-side history caps: the server re-caps and re-redacts, but the control
 *  end bounds the replayed payload too (last N turns, each truncated). */
const MAX_HISTORY_TURNS = 6;
const MAX_HISTORY_MSG_CHARS = 2000;

type UseTerminalCopilotProps = {
    /** The target host's connection id; the ask rides the outer
     *  `to_connection_id`, resolved + authorized server-side (never in payload). */
    connectionId: string | null;
    subscribe: (handler: SignalingSubscriber) => () => void;
    sendMessage: (
        type: number,
        data: unknown,
        connectionId?: string,
        requestId?: string,
    ) => string;
};

export type AskInput = {
    mode: TerminalCopilotMode;
    /** `how_to`: the operator's natural-language request. */
    question?: string;
    context: TerminalContext;
    /** Manager-only user-selected agent model. Omitted (null/undefined) when the
     *  model selector is hidden (open-source signal); the server then resolves the
     *  default, so the flow is identical across both signaling targets. */
    modelId?: number | null;
    /** Manager-only active-organization hint. Set only in the console's org view;
     *  omitted by the personal view and the open-source control end, so no `org_id`
     *  rides the wire and the ask resolves personal-scoped exactly as before. */
    orgId?: number;
};

/**
 * Drives the in-terminal AI copilot over control-plane signaling: sends a
 * `TerminalCopilotAsk`, aggregates the notification-style `TerminalCopilotEvent`
 * stream by `request_id` (ordered by `seq`, ignoring stale/replayed frames), and
 * exposes a reset that cancels the run. The two terminal-page connections are
 * deliberately separate: the copilot rides the desk signaling WS, the shell I/O
 * rides its own `/api/desk/terminal/{id}` WS.
 */
export function useTerminalCopilot({
    connectionId,
    subscribe,
    sendMessage,
}: UseTerminalCopilotProps) {
    const { i18n } = useTranslation();
    const [state, setState] = useState<CopilotState>(INITIAL_STATE);
    const activeRequestRef = useRef<string | null>(null);
    const conversationIdRef = useRef<string | null>(null);
    // Highest applied seq, so duplicate / out-of-order frames cannot corrupt the
    // accumulated text. Reset per run (frames start at seq 0).
    const lastSeqRef = useRef<number>(-1);
    // Mirror of the rendered conversation, read at ask time to build the replayed
    // history (kept in a ref so `ask` need not depend on `state`).
    const turnsRef = useRef<TerminalCopilotTurn[]>([]);
    useEffect(() => {
        turnsRef.current = state.turns;
    }, [state.turns]);

    const ask = useCallback(
        (input: AskInput) => {
            if (!connectionId) return;
            if (!conversationIdRef.current) conversationIdRef.current = v4();
            const conversationId = conversationIdRef.current;
            // The operator's message for this turn: their request (`how_to`) or the
            // error passage (`explain_error`). Drives the user bubble and history.
            const question = (
                input.mode === 'how_to'
                    ? (input.question ?? '')
                    : (input.context.error_text ?? '')
            ).trim();
            // Replay the prior completed turns so the stateless central brain can
            // continue the thread (the manager ignores this and uses its DB session).
            const history: CopilotHistoryTurn[] = turnsRef.current
                .filter((t) => t.answer)
                .slice(-MAX_HISTORY_TURNS)
                .map((t) => ({
                    user: t.question.slice(0, MAX_HISTORY_MSG_CHARS),
                    assistant: (t.answer?.explanation_md ?? '').slice(0, MAX_HISTORY_MSG_CHARS),
                }));
            const data = {
                conversation_id: conversationId,
                mode: input.mode,
                question: input.question,
                // The server steers the answer language by the operator's UI
                // locale; it is a non-authoritative hint, re-validated server-side.
                locale: i18n.language,
                history,
                context: input.context,
                // Absent (undefined) unless the manager model selector supplied a
                // choice; the open-source server ignores the field anyway.
                model_id: input.modelId ?? undefined,
                // Absent (undefined) unless the console's org view supplied it; a
                // non-authoritative hint the manager validates, ignored open-source.
                org_id: input.orgId ?? undefined,
            };
            const requestId = sendMessage(
                SIGNALING_TYPE_CODE_TERMINAL_COPILOT_ASK,
                data,
                connectionId,
            );
            activeRequestRef.current = requestId;
            lastSeqRef.current = -1;
            setState((prev) => ({
                ...prev,
                phase: 'running',
                requestId,
                conversationId,
                mode: input.mode,
                // Append the in-flight turn; its answer fills in on the Final frame.
                turns: [
                    ...prev.turns,
                    { question, mode: input.mode, answer: null, provenance: null },
                ],
                partialText: '',
                committedText: '',
                tools: [],
                error: null,
                errorCode: null,
                retractionReason: null,
            }));
        },
        [connectionId, sendMessage, i18n],
    );

    // Stop tracking the current run and clear the panel. If a run is still in
    // flight, notify the server (correlated by the original ask id) so it can
    // settle the orphaned turn; the next ask opens a fresh conversation.
    const reset = useCallback(() => {
        const requestId = activeRequestRef.current;
        if (connectionId && requestId) {
            sendMessage(
                SIGNALING_TYPE_CODE_TERMINAL_COPILOT_CANCEL,
                null,
                connectionId,
                requestId,
            );
        }
        activeRequestRef.current = null;
        conversationIdRef.current = null;
        lastSeqRef.current = -1;
        turnsRef.current = [];
        setState(INITIAL_STATE);
    }, [connectionId, sendMessage]);

    useEffect(() => {
        const handle = (message: SignalingMessage) => {
            if (message.signaling_type !== SIGNALING_TYPE_CODE_TERMINAL_COPILOT_EVENT) return;
            const event = message.signaling_data as TerminalCopilotEvent | null;
            if (!event || event.request_id !== activeRequestRef.current) return;
            // Ignore stale / replayed frames.
            if (event.seq <= lastSeqRef.current) return;
            lastSeqRef.current = event.seq;

            setState((prev) => {
                switch (event.kind) {
                    case 'partial':
                        return {
                            ...prev,
                            partialText: prev.partialText + (event.partial_text ?? ''),
                        };
                    case 'partial_committed':
                        return {
                            ...prev,
                            committedText: prev.committedText + prev.partialText,
                            partialText: '',
                        };
                    case 'retracted':
                        activeRequestRef.current = null;
                        return {
                            ...prev,
                            phase: 'error',
                            partialText: '',
                            error: event.error?.message ?? 'content retracted',
                            errorCode: event.error?.error_code ?? null,
                            retractionReason: event.retraction_reason ?? 'incomplete',
                        };
                    case 'tool_started':
                        return {
                            ...prev,
                            tools: [...prev.tools, { name: event.tool_name ?? 'tool' }],
                        };
                    case 'final': {
                        activeRequestRef.current = null;
                        // Commit the structured answer into the in-flight (last) turn.
                        const answer = event.answer ?? { explanation_md: '', suggestions: [] };
                        const turns = prev.turns.slice();
                        if (turns.length) {
                            turns[turns.length - 1] = {
                                ...turns[turns.length - 1],
                                answer,
                                provenance: event.provenance ?? null,
                            };
                        }
                        return {
                            ...prev,
                            phase: 'done',
                            partialText: '',
                            committedText: '',
                            retractionReason: null,
                            turns,
                        };
                    }
                    case 'error':
                        activeRequestRef.current = null;
                        // The in-flight turn keeps a null answer; the panel shows the
                        // error for it. It is filtered out of any replayed history.
                        return {
                            ...prev,
                            phase: 'error',
                            error: event.error?.message ?? 'copilot failed',
                            retractionReason: event.retraction_reason ?? null,
                            errorCode: event.error?.error_code ?? null,
                        };
                    default:
                        return prev;
                }
            });
        };
        return subscribe(handle);
    }, [subscribe]);

    return { state, ask, reset };
}
