import { useCallback, useEffect, useRef, useState } from 'react';
import {
    SIGNALING_TYPE_CODE_CONFIRM_EXEC,
    SIGNALING_TYPE_CODE_EXEC_PREVIEW,
    SIGNALING_TYPE_CODE_EXEC_RESULT,
    SIGNALING_TYPE_CODE_RESOLVE_EXEC,
} from '../desk/constants';
import type { SignalingMessage, SignalingSubscriber } from '../desk/use-desk-signaling';

// Wire types — mirror `desk_agent_protocol::exec`. These ride the ConfirmExec /
// ExecPreview / ResolveExec / ExecResult signaling types as `signaling_data`;
// like the diagnose types they are not part of the REST OpenAPI surface.
//
// This hook is feature-neutral: both the diagnose panel and the terminal copilot
// drive the same sealed confirm-exec lifecycle through it. It does not depend on
// any feature's suggestion shape — callers map their own suggestion into the
// neutral `ExecRequestInput`.

export type RiskLevel = 'low' | 'medium' | 'high' | 'critical' | 'blocked';

export type AgentError = {
    kind: string;
    message: string;
    retryable: boolean;
    safe_for_model: boolean;
};

export type ExecPreview = {
    exec_request_id: string | null;
    shell: string;
    command: string;
    cwd: string | null;
    timeout_ms: number;
    risk: RiskLevel;
    impact: string;
    policy_note: string | null;
    requires_confirmation: boolean;
    executable: boolean;
    blocked_reason: string | null;
};

export type ExecOutput = {
    exit_code: number;
    stdout: string;
    stderr: string;
    stdout_truncated: boolean;
    stderr_truncated: boolean;
    duration_ms: number;
    redactions: string[];
};

// `AgentOutcome` is serde-tagged `{ status: 'ok' | 'err', data }`.
export type ExecOutcome =
    | { status: 'ok'; data: { kind: 'exec'; params: ExecOutput } | unknown }
    | { status: 'err'; data: AgentError };

export type ExecResultPayload = {
    exec_request_id: string;
    outcome: ExecOutcome;
};

// One execution's lifecycle, keyed by the row it belongs to.
export type ExecPhase = 'previewing' | 'awaiting' | 'running' | 'done' | 'error';

export type ExecEntry = {
    phase: ExecPhase;
    preview: ExecPreview | null;
    /** Server-minted id once a preview is executable / approved. */
    execRequestId: string | null;
    /** Result output on success. */
    output: ExecOutput | null;
    /** Human-readable error (preview-blocked, or execution failure). */
    error: string | null;
};

/**
 * A command to ask the host to classify and (on approval) run. Feature-neutral:
 * the diagnose panel maps a `SuggestedCommand` here (with `cwd: null`, since a
 * diagnosis carries no working directory), and the terminal copilot maps a
 * `CommandSuggestion` here, preserving the suggestion's own `cwd`.
 */
export type ExecRequestInput = {
    shell: string;
    command: string;
    cwd: string | null;
    reason: string;
};

/** Decoded exec output from an outcome, or null if it was an error / non-exec. */
function execOutputFromOutcome(outcome: ExecOutcome): ExecOutput | null {
    if (outcome.status !== 'ok') return null;
    const data = outcome.data as { kind?: string; params?: ExecOutput };
    if (data && data.kind === 'exec' && data.params) return data.params;
    return null;
}

type UseConfirmExecProps = {
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
 * Drives confirmed execution of a command over signaling: sends ConfirmExec,
 * tracks the ExecPreview, and on user approval sends ResolveExec and backfills
 * the ExecResult — all keyed by the caller's row index so each row shows its own
 * state. The server is the source of truth: it classifies, mints the
 * `exec_request_id`, and only previews/executes whitelist templates. The host
 * re-runs classification on the relayed command, so a control-end-reported
 * decision is never trusted.
 */
export function useConfirmExec({ deskId, subscribe, sendMessage }: UseConfirmExecProps) {
    // Keyed by the caller's row index.
    const [entries, setEntries] = useState<Record<number, ExecEntry>>({});
    // Map an in-flight ConfirmExec signaling request_id -> row index, so the
    // ExecPreview frame (correlated by that request_id) lands on the right row.
    const previewReqToRow = useRef<Record<string, number>>({});
    // Map exec_request_id -> row index, so an ExecResult backfills the right row.
    const execIdToRow = useRef<Record<string, number>>({});

    const requestPreview = useCallback(
        (rowIndex: number, input: ExecRequestInput) => {
            if (!deskId) return;
            const data = {
                operation: {
                    risk_hint: null,
                    input: {
                        kind: 'exec',
                        params: {
                            target: { type: 'shell', shell: input.shell },
                            command: input.command,
                            cwd: input.cwd,
                            timeout_ms: 0,
                            max_stdout_bytes: 0,
                            max_stderr_bytes: 0,
                        },
                    },
                },
                reason: input.reason,
            };
            const requestId = sendMessage(SIGNALING_TYPE_CODE_CONFIRM_EXEC, data, deskId);
            previewReqToRow.current[requestId] = rowIndex;
            setEntries((prev) => ({
                ...prev,
                [rowIndex]: {
                    phase: 'previewing',
                    preview: null,
                    execRequestId: null,
                    output: null,
                    error: null,
                },
            }));
        },
        [deskId, sendMessage],
    );

    const approve = useCallback(
        (rowIndex: number) => {
            const entry = entries[rowIndex];
            if (!deskId || !entry?.execRequestId) return;
            sendMessage(
                SIGNALING_TYPE_CODE_RESOLVE_EXEC,
                { exec_request_id: entry.execRequestId, decision: 'approve' },
                deskId,
            );
            setEntries((prev) => ({
                ...prev,
                [rowIndex]: { ...prev[rowIndex], phase: 'running' },
            }));
        },
        [deskId, entries, sendMessage],
    );

    const reject = useCallback(
        (rowIndex: number) => {
            const entry = entries[rowIndex];
            if (deskId && entry?.execRequestId) {
                sendMessage(
                    SIGNALING_TYPE_CODE_RESOLVE_EXEC,
                    { exec_request_id: entry.execRequestId, decision: 'reject' },
                    deskId,
                );
            }
            setEntries((prev) => {
                const next = { ...prev };
                delete next[rowIndex];
                return next;
            });
        },
        [deskId, entries, sendMessage],
    );

    const dismiss = useCallback((rowIndex: number) => {
        setEntries((prev) => {
            const next = { ...prev };
            delete next[rowIndex];
            return next;
        });
    }, []);

    useEffect(() => {
        // Subscribe to the lossless signaling stream: every ExecPreview /
        // ExecResult is delivered in order, so two frames arriving in one
        // tick can no longer coalesce away.
        const handle = (message: SignalingMessage) => {
            if (message.signaling_type === SIGNALING_TYPE_CODE_EXEC_PREVIEW) {
                const preview = message.signaling_data as ExecPreview | null;
                const reqId = message.request_id;
                if (!preview || !reqId) return;
                const rowIndex = previewReqToRow.current[reqId];
                if (rowIndex === undefined) return;
                delete previewReqToRow.current[reqId];
                if (preview.exec_request_id) {
                    execIdToRow.current[preview.exec_request_id] = rowIndex;
                }
                setEntries((prev) => ({
                    ...prev,
                    [rowIndex]: {
                        phase: preview.executable ? 'awaiting' : 'error',
                        preview,
                        execRequestId: preview.exec_request_id,
                        output: null,
                        error: preview.executable
                            ? null
                            : (preview.blocked_reason ?? preview.policy_note ?? preview.impact),
                    },
                }));
                return;
            }

            if (message.signaling_type === SIGNALING_TYPE_CODE_EXEC_RESULT) {
                const payload = message.signaling_data as ExecResultPayload | null;
                if (!payload) return;
                const rowIndex = execIdToRow.current[payload.exec_request_id];
                if (rowIndex === undefined) return;
                delete execIdToRow.current[payload.exec_request_id];
                const output = execOutputFromOutcome(payload.outcome);
                setEntries((prev) => ({
                    ...prev,
                    [rowIndex]: {
                        ...prev[rowIndex],
                        phase: output ? 'done' : 'error',
                        output,
                        error:
                            output || payload.outcome.status !== 'err'
                                ? null
                                : payload.outcome.data.message,
                    },
                }));
            }
        };
        return subscribe(handle);
    }, [subscribe]);

    return { entries, requestPreview, approve, reject, dismiss };
}
