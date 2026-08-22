import { useCallback, useEffect, useRef, useState } from 'react';
import {
    SIGNALING_TYPE_CODE_PREVIEW_EXECUTION,
    SIGNALING_TYPE_CODE_CONTROL_EXECUTION,
    SIGNALING_TYPE_CODE_EXECUTION_PROGRESS_UPDATED,
    SIGNALING_TYPE_CODE_EXECUTION_PREVIEW_GENERATED,
    SIGNALING_TYPE_CODE_EXECUTION_COMPLETED,
    SIGNALING_TYPE_CODE_EXECUTION_STATE_REPORTED,
    SIGNALING_TYPE_CODE_RESOLVE_EXECUTION,
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
    /** Optional business code (a `DeskErrorCode`) the control end localizes. */
    error_code?: number | null;
};

export type ExecPreview = {
    exec_request_id: string | null;
    shell: string;
    command: string;
    cwd: string | null;
    approval_timeout_ms: number;
    timeout_ms: number;
    risk: RiskLevel;
    /** Server-authoritative admission basis; absent on an older server. */
    execution_basis?: "template" | "owner_blocklist_only";
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

// Wire types — mirror `desk_agent_protocol::exec_lifecycle`.

/** What the host's own ledger says about one dispatch. */
export type ExecState =
    | 'reserved'
    | 'running'
    | 'terminal'
    | 'indeterminate'
    | 'spawn_failed'
    | 'unknown';

export type ExecLifecyclePayload = {
    execution_generation: string;
} & (
    | { event: 'accepted'; containment_identity: string | null }
    | { event: 'heartbeat'; running_ms: number }
);

export type ExecStateReplyPayload = {
    execution_generation: string;
    state: ExecState;
    containment_identity: string | null;
    running_ms: number | null;
    detail: string | null;
};

/** Browser wire shape of the flattened Rust `ExecControlPayload`. */
export type ExecControlPayload =
    | {
          execution_generation: string;
          action: 'cancel';
          requested_by: string;
      }
    | {
          execution_generation: string;
          action: 'query_state';
      };

/** Whether a state settles the execution, so there is nothing left to wait for. */
function isSettled(state: ExecState): boolean {
    return state === 'terminal' || state === 'indeterminate' || state === 'spawn_failed';
}

// One execution's lifecycle, keyed by the row it belongs to.
//
// `dispatching` and `running` are deliberately distinct. Approving used to put a
// row straight into `running`, which was a guess: the host had not said anything
// yet, and a command that never started still looked like one that was working.
// `running` is now only ever entered because the host said it had started.
export type ExecPhase =
    | 'previewing'
    | 'awaiting'
    | 'dispatching'
    | 'running'
    | 'done'
    | 'error';

export type ExecEntry = {
    phase: ExecPhase;
    preview: ExecPreview | null;
    /** Server-minted id once a preview is executable / approved. */
    execRequestId: string | null;
    /** The id of the one dispatch, known from the frame that approved it. Cancel
     *  and state queries name this rather than the task, so they can never hit a
     *  retry of the same command. */
    executionGeneration: string | null;
    /** How long the host says it has been running, from its own clock. */
    runningMs: number | null;
    /** Set once a stop has been asked for; the command is not over until the host
     *  says so, so this is shown alongside the phase rather than replacing it. */
    cancelRequested: boolean;
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
    /** Manager-only active-organization hint. Set only in the console's org view;
     *  omitted (undefined) by the personal view and the open-source control end, so
     *  no `org_id` rides the wire and the request resolves against the personal
     *  subject. The manager validates it (membership + the org's device-access
     *  grant) before adjudicating the exec against that single org, and a non-owner
     *  must carry it — the server otherwise denies with a generic permission error. */
    orgId?: number;
};

/**
 * Drives confirmed execution of a command over signaling: sends ConfirmExec,
 * tracks the ExecPreview, and on user approval sends ResolveExec and backfills
 * the ExecResult — all keyed by the caller's row index so each row shows its own
 * state. The server is the source of truth: it classifies, mints the
 * `exec_request_id`, and executes only server-admitted previews. The host
 * re-runs classification on the relayed command, so a control-end-reported
 * decision is never trusted.
 */
export function useConfirmExec({ deskId, subscribe, sendMessage, orgId }: UseConfirmExecProps) {
    // Keyed by the caller's row index.
    const [entries, setEntries] = useState<Record<number, ExecEntry>>({});
    // Map an in-flight ConfirmExec signaling request_id -> row index, so the
    // ExecPreview frame (correlated by that request_id) lands on the right row.
    const previewReqToRow = useRef<Record<string, number>>({});
    // Map exec_request_id -> row index, so an ExecResult backfills the right row.
    const execIdToRow = useRef<Record<string, number>>({});
    // Map execution generation -> row index, so the host's lifecycle frames and
    // state replies land on the row that asked for that one dispatch.
    const generationToRow = useRef<Record<string, number>>({});

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
                // Manager-only org hint; undefined omits it from the wire so the
                // open-source single-instance desk-server (which ignores it) and the
                // personal view behave identically.
                org_id: orgId ?? undefined,
            };
            const requestId = sendMessage(SIGNALING_TYPE_CODE_PREVIEW_EXECUTION, data, deskId);
            previewReqToRow.current[requestId] = rowIndex;
            setEntries((prev) => ({
                ...prev,
                [rowIndex]: {
                    phase: 'previewing',
                    preview: null,
                    execRequestId: null,
                    executionGeneration: null,
                    runningMs: null,
                    cancelRequested: false,
                    output: null,
                    error: null,
                },
            }));
        },
        [deskId, sendMessage, orgId],
    );

    const approve = useCallback(
        (rowIndex: number) => {
            const entry = entries[rowIndex];
            if (!deskId || !entry?.execRequestId) return;
            // The frame that approves is the one that triggers the dispatch, so
            // its id is the execution generation the host will report against.
            const generation = sendMessage(
                SIGNALING_TYPE_CODE_RESOLVE_EXECUTION,
                { exec_request_id: entry.execRequestId, decision: 'approve' },
                deskId,
            );
            generationToRow.current[generation] = rowIndex;
            setEntries((prev) => ({
                ...prev,
                [rowIndex]: {
                    ...prev[rowIndex],
                    // Not `running`: approving is a request, and only the host can
                    // say whether the command actually started.
                    phase: 'dispatching',
                    executionGeneration: generation,
                },
            }));
        },
        [deskId, entries, sendMessage],
    );

    const reject = useCallback(
        (rowIndex: number) => {
            const entry = entries[rowIndex];
            if (deskId && entry?.execRequestId) {
                sendMessage(
                    SIGNALING_TYPE_CODE_RESOLVE_EXECUTION,
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

    /** Ask the host to stop a running command and reclaim its process tree. */
    const cancel = useCallback(
        (rowIndex: number) => {
            const entry = entries[rowIndex];
            if (!deskId || !entry?.executionGeneration) return;
            const payload: ExecControlPayload = {
                execution_generation: entry.executionGeneration,
                action: 'cancel',
                requested_by: 'control-end',
            };
            sendMessage(SIGNALING_TYPE_CODE_CONTROL_EXECUTION, payload, deskId);
            // The row is not moved to a finished phase: a stop that was asked for
            // is not a stop that happened, and only the host's own result says
            // whether — and how far — the command ran.
            setEntries((prev) => ({
                ...prev,
                [rowIndex]: { ...prev[rowIndex], cancelRequested: true },
            }));
        },
        [deskId, entries, sendMessage],
    );

    /** Ask the host what it currently believes about this dispatch. */
    const queryState = useCallback(
        (rowIndex: number) => {
            const entry = entries[rowIndex];
            if (!deskId || !entry?.executionGeneration) return;
            const payload: ExecControlPayload = {
                execution_generation: entry.executionGeneration,
                action: 'query_state',
            };
            sendMessage(SIGNALING_TYPE_CODE_CONTROL_EXECUTION, payload, deskId);
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
            if (message.signaling_type === SIGNALING_TYPE_CODE_EXECUTION_PREVIEW_GENERATED) {
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
                        ...prev[rowIndex],
                        phase: preview.executable ? 'awaiting' : 'error',
                        preview,
                        execRequestId: preview.exec_request_id,
                        executionGeneration: null,
                        runningMs: null,
                        cancelRequested: false,
                        output: null,
                        error: preview.executable
                            ? null
                            : preview.blocked_reason,
                    },
                }));
                return;
            }

            if (message.signaling_type === SIGNALING_TYPE_CODE_EXECUTION_PROGRESS_UPDATED) {
                const payload = message.signaling_data as ExecLifecyclePayload | null;
                if (!payload) return;
                const rowIndex = generationToRow.current[payload.execution_generation];
                if (rowIndex === undefined) return;
                setEntries((prev) => {
                    const entry = prev[rowIndex];
                    // A progress frame arriving after the result must not reopen a
                    // finished row; the terminal answer already settled it.
                    if (!entry || entry.phase === 'done' || entry.phase === 'error') {
                        return prev;
                    }
                    return {
                        ...prev,
                        [rowIndex]: {
                            ...entry,
                            phase: 'running',
                            runningMs:
                                payload.event === 'heartbeat'
                                    ? payload.running_ms
                                    : entry.runningMs,
                        },
                    };
                });
                return;
            }

            if (message.signaling_type === SIGNALING_TYPE_CODE_EXECUTION_STATE_REPORTED) {
                const payload = message.signaling_data as ExecStateReplyPayload | null;
                if (!payload) return;
                const rowIndex = generationToRow.current[payload.execution_generation];
                if (rowIndex === undefined) return;
                setEntries((prev) => {
                    const entry = prev[rowIndex];
                    if (!entry || entry.phase === 'done' || entry.phase === 'error') {
                        return prev;
                    }
                    // A settled state the result has not delivered means the host
                    // has no answer to give — it lost track of the command, or the
                    // spawn failed. Say so rather than waiting for a result that is
                    // never coming.
                    if (isSettled(payload.state) && payload.state !== 'terminal') {
                        return {
                            ...prev,
                            [rowIndex]: {
                                ...entry,
                                phase: 'error',
                                error: payload.detail ?? payload.state,
                            },
                        };
                    }
                    return {
                        ...prev,
                        [rowIndex]: {
                            ...entry,
                            phase: payload.state === 'running' ? 'running' : entry.phase,
                            runningMs: payload.running_ms ?? entry.runningMs,
                        },
                    };
                });
                return;
            }

            if (message.signaling_type === SIGNALING_TYPE_CODE_EXECUTION_COMPLETED) {
                const payload = message.signaling_data as ExecResultPayload | null;
                if (!payload) return;
                const rowIndex = execIdToRow.current[payload.exec_request_id];
                if (rowIndex === undefined) return;
                delete execIdToRow.current[payload.exec_request_id];
                const output = execOutputFromOutcome(payload.outcome);
                // The dispatch is over, so stop routing its lifecycle frames.
                for (const [generation, row] of Object.entries(generationToRow.current)) {
                    if (row === rowIndex) delete generationToRow.current[generation];
                }
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

    return { entries, requestPreview, approve, reject, cancel, queryState, dismiss };
}
