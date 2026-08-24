import type { AiProvenance } from "@/components/ai-generated-mark";
import type { ExecPreview } from "../exec/use-confirm-exec";

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
    | 'partial_committed'
    | 'retracted'
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
export type StreamRetractionReason =
    | 'policy_blocked'
    | 'safe_redirect'
    | 'safety_unavailable'
    | 'incomplete';

export type DiagnoseEvent = {
    request_id: string;
    seq: number;
    kind: DiagnoseEventKind;
    status?: string | null;
    partial_summary?: string | null;
    final_result?: Diagnosis | null;
    /** `retracted`: fixed local policy/unavailable message selector. */
    retraction_reason?: StreamRetractionReason | null;
    error?: AgentError | null;
    /** `turn_started`: the id of the agentic turn that started. */
    turn_id?: string | null;
    /** `context_compacted`: committed checkpoint generation. */
    checkpoint_generation?: number | null;
    /** `context_compacted`: original messages covered by the checkpoint. */
    covered_message_count?: number | null;
    /** `tool_started`: the model-facing tool name. */
    tool_name?: string | null;
    /** `tool_started`: the raw JSON arguments produced by the model. */
    tool_arguments_json?: string | null;
    /** `tool_started` / `tool_finished`: the tool call id. */
    tool_call_id?: string | null;
    /** `tool_started`: a mutating tool waiting for the operator's approval. */
    awaiting_approval?: boolean;
    /** `tool_finished`: whether the call produced a usable result. */
    tool_ok?: boolean | null;
    /** `tool_finished`: redacted, bounded output returned to the model. */
    tool_output?: string | null;
    /** `tool_finished`: stable id when execution continues in the background. */
    background_task_id?: string | null;
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
    argumentsJson: string;
    output: string | null;
    backgroundTaskId: string | null;
};

/** One visible item in an agentic turn, kept in backend message order. */
export type DiagnoseTimelineItem =
    | {
          kind: 'context_notice';
          id: string;
          turnId: string;
          noticeKind: 'trimmed' | 'compacted';
      }
    | {
          kind: 'assistant';
          id: string;
          text: string;
          provenance: AiProvenance | null;
      }
    | {
          kind: 'tool';
          id: string;
          activity: ToolActivity;
      }
    | {
          kind: 'background_completion';
          id: string;
          toolCallId: string | null;
          taskId: string | null;
          output: string;
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
// `final`, `error` on a terminal `error` frame.
export type DiagnosePhase = 'idle' | 'running' | 'done' | 'error';

/**
 * One settled turn of the conversation, frozen for the transcript once a newer
 * follow-up question starts. The live (current) turn is held in the top-level
 * state fields; when the next `start` begins, the settled live turn is snapshot
 * into `history` so the panel can render the running conversation.
 */
export type DiagnoseHistoryTurn = {
    requestId: string;
    turnId: string | null;
    /** The question the user asked for this turn. */
    question: string;
    /** Structured result, if a `final` frame arrived (single-turn path). */
    result: Diagnosis | null;
    /** Streaming summary captured for this turn (fallback display text). */
    summary: string;
    /** Assistant messages and tool calls in their original order. */
    timeline: DiagnoseTimelineItem[];
    /** How the turn settled. */
    phase: 'done' | 'error';
    /** Failure message if the turn errored. */
    error: string | null;
    /** Stable business code used to localize the failure. */
    errorCode: number | null;
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
     * `start`, regenerated on a desk change / `reset`.
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
    /** Assistant messages and tool calls in their original order. */
    timeline: DiagnoseTimelineItem[];
    /** Closed reason for a server-requested provisional-text retraction. */
    retractionReason?: StreamRetractionReason | null;
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
    /** A durable background command still running on the target. */
    backgroundExecution: {
        executionGeneration: string;
        cancelRequested: boolean;
    } | null;
    /** Prior settled turns of this conversation, oldest first. */
    history: DiagnoseHistoryTurn[];
};

export const INITIAL_STATE: DiagnoseState = {
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
    timeline: [],
    provenance: null,
    pendingExec: null,
    backgroundExecution: null,
    history: [],
};

/**
 * Freeze the previous live turn into a transcript entry when a follow-up turn
 * begins. Only a settled (`done` / `error`) turn is captured; starting the very
 * first turn from `idle` adds nothing.
 */
export function snapshotLiveTurn(prev: DiagnoseState): DiagnoseHistoryTurn[] {
    if ((prev.phase !== 'done' && prev.phase !== 'error') || !prev.requestId) {
        return prev.history;
    }
    const summary = extractStreamingSummary(prev.partialSummary);
    const timeline =
        !prev.result && summary
            ? [
                  ...prev.timeline,
                  {
                      kind: 'assistant' as const,
                      id: `assistant:${prev.requestId}:settled`,
                      text: summary,
                      provenance: prev.provenance,
                  },
              ]
            : prev.timeline;
    return [
        ...prev.history,
        {
            requestId: prev.requestId,
            turnId: prev.turnId,
            question: prev.question,
            result: prev.result,
            summary,
            timeline,
            phase: prev.phase,
            error: prev.error,
            errorCode: prev.errorCode,
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
    turnId?: string | null;
    role: 'user' | 'assistant' | 'tool' | 'system_event' | 'untrusted_output' | 'system';
    text: string;
    toolCalls?: { id: string; name: string; argumentsJson: string }[];
    toolCallId?: string | null;
    backgroundTaskId?: string | null;
};

/** A settled snapshot: the persisted transcript plus a monotonic sequence. */
export type SessionSnapshot = {
    seq: number;
    active: boolean;
    requestId?: string | null;
    activeExecutionGeneration?: string | null;
    messages: SnapshotMessage[];
    contextNotices: {
        id: string;
        turnId: string;
        kind: 'trimmed' | 'compacted';
        checkpointGeneration?: number | null;
        coveredMessageCount?: number | null;
    }[];
};

/** One authorized history-list row for the current target device. */
export type DiagnoseSessionSummary = {
    sessionId: string;
    conversationId?: string | null;
    firstQuestion?: string | null;
    createdAt: string;
    updatedAt: string;
    active: boolean;
    messageCount: number;
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
 * turns at each `user` message. Assistant text, tool calls, and background
 * completion messages remain separate timeline items in their original order.
 * A direct tool result updates its call card; a later background completion is
 * appended as its own event before the automatic assistant follow-up.
 */
export function buildSnapshotTranscript(
    messages: SnapshotMessage[],
    contextNotices: SessionSnapshot['contextNotices'] = [],
): DiagnoseHistoryTurn[] {
    const turns: DiagnoseHistoryTurn[] = [];
    const open = (id: string, question: string, turnId: string | null): DiagnoseHistoryTurn => ({
        requestId: id,
        turnId,
        question,
        result: null,
        summary: '',
        timeline: [],
        phase: 'done',
        error: null,
        errorCode: null,
        provenance: null,
    });
    let current: DiagnoseHistoryTurn | null = null;
    for (const m of messages) {
        if (m.role === 'user') {
            if (current) turns.push(current);
            current = open(m.id, m.text, m.turnId ?? null);
        } else if (m.role === 'assistant') {
            if (!current) current = open(m.id, '', m.turnId ?? null);
            if (m.text) {
                current.timeline.push({
                    kind: 'assistant',
                    id: m.id,
                    text: m.text,
                    provenance: null,
                });
            }
            for (const tc of m.toolCalls ?? []) {
                current.timeline.push({
                    kind: 'tool',
                    id: tc.id,
                    activity: {
                        callId: tc.id,
                        name: tc.name,
                        status: 'ok',
                        argumentsJson: tc.argumentsJson,
                        output: null,
                        backgroundTaskId: null,
                    },
                });
            }
        } else if (m.role === 'tool' && current && m.toolCallId) {
            current.timeline = current.timeline.map((item) =>
                item.kind === 'tool' && item.activity.callId === m.toolCallId
                    ? {
                          ...item,
                          activity: {
                              ...item.activity,
                              status: 'ok',
                              output: m.text,
                              backgroundTaskId:
                                  m.backgroundTaskId ?? item.activity.backgroundTaskId,
                          },
                      }
                    : item,
            );
        } else if (m.role === 'untrusted_output' && current) {
            current.timeline.push({
                kind: 'background_completion',
                id: m.id,
                toolCallId: m.toolCallId ?? null,
                taskId: m.backgroundTaskId ?? null,
                output: m.text,
            });
        }
        // Unlinked internal/system messages carry no user-facing turn text.
    }
    if (current) turns.push(current);
    for (const notice of contextNotices) {
        if (notice.kind !== 'trimmed' && notice.kind !== 'compacted') continue;
        const turn = turns.find(candidate => candidate.turnId === notice.turnId);
        if (!turn || turn.timeline.some(item => item.id === notice.id)) continue;
        turn.timeline.unshift({
            kind: 'context_notice',
            id: notice.id,
            turnId: notice.turnId,
            noticeKind: notice.kind,
        });
    }
    return turns;
}
