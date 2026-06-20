import { useCallback, useEffect, useRef, useState } from 'react';
import {
    SIGNALING_TYPE_CODE_DIAGNOSE,
    SIGNALING_TYPE_CODE_DIAGNOSE_EVENT,
    SIGNALING_TYPE_CODE_DIAGNOSE_CANCEL,
} from './constants';
import type { SignalingMessage } from './use-desk-signaling';

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

export type DiagnoseEventKind = 'status' | 'partial' | 'final' | 'error';

export type AgentError = {
    kind: string;
    message: string;
    retryable: boolean;
    safe_for_model: boolean;
};

export type DiagnoseEvent = {
    request_id: string;
    seq: number;
    kind: DiagnoseEventKind;
    status?: string | null;
    partial_summary?: string | null;
    final_result?: Diagnosis | null;
    error?: AgentError | null;
};

export type DiagnoseStartOptions = {
    includeScreen?: boolean;
    contextKinds?: string[];
    /** BCP-47 tag of the current UI language, so the AI answers in it. */
    locale?: string;
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

export type DiagnoseState = {
    phase: DiagnosePhase;
    requestId: string | null;
    /** Latest lifecycle phase name (collecting / redacting / modeling). */
    status: string | null;
    /** Accumulated streaming summary fragments. */
    partialSummary: string;
    /** The structured result, set on a `final` frame. */
    result: Diagnosis | null;
    /** A human-readable failure message, set on an `error` frame. */
    error: string | null;
};

const INITIAL_STATE: DiagnoseState = {
    phase: 'idle',
    requestId: null,
    status: null,
    partialSummary: '',
    result: null,
    error: null,
};

type UseDeskDiagnoseProps = {
    deskId: string | null;
    lastMessage: SignalingMessage | null;
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
export function useDeskDiagnose({ deskId, lastMessage, sendMessage }: UseDeskDiagnoseProps) {
    const [state, setState] = useState<DiagnoseState>(INITIAL_STATE);
    const activeRequestRef = useRef<string | null>(null);
    // Highest applied seq, so duplicate / out-of-order frames cannot corrupt
    // the accumulated summary. Reset to -1 per run (frames start at seq 0).
    const lastSeqRef = useRef<number>(-1);

    const start = useCallback(
        (question: string, options?: DiagnoseStartOptions) => {
            if (!deskId) return;
            const data = {
                question,
                include_screen: options?.includeScreen ?? false,
                context_kinds: options?.contextKinds ?? [],
                locale: options?.locale,
            };
            const requestId = sendMessage(SIGNALING_TYPE_CODE_DIAGNOSE, data, deskId);
            activeRequestRef.current = requestId;
            lastSeqRef.current = -1;
            setState({ ...INITIAL_STATE, phase: 'running', requestId });
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
        setState((prev) => ({ ...prev, phase: 'done' }));
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
        lastSeqRef.current = -1;
        setState(INITIAL_STATE);
    }, [deskId, sendMessage]);

    useEffect(() => {
        if (!lastMessage) return;
        if (lastMessage.signaling_type !== SIGNALING_TYPE_CODE_DIAGNOSE_EVENT) return;
        const event = lastMessage.signaling_data as DiagnoseEvent | null;
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
                    return { ...prev, phase: 'done', result: event.final_result ?? null };
                case 'error':
                    activeRequestRef.current = null;
                    return {
                        ...prev,
                        phase: 'error',
                        error: event.error?.message ?? 'diagnosis failed',
                    };
                default:
                    return prev;
            }
        });
    }, [lastMessage]);

    return { state, start, handoff, reset };
}
