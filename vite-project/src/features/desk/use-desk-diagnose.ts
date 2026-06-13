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
};

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

    // Full reset back to the question form.
    const reset = useCallback(() => {
        activeRequestRef.current = null;
        lastSeqRef.current = -1;
        setState(INITIAL_STATE);
    }, []);

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
