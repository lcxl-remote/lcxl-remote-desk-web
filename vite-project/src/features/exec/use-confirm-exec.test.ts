import { describe, it, expect, vi, beforeEach } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useConfirmExec } from './use-confirm-exec';
import type {
    ExecLifecyclePayload,
    ExecPreview,
    ExecRequestInput,
    ExecResultPayload,
    ExecStateReplyPayload,
} from './use-confirm-exec';
import type { SignalingMessage } from '../desk/use-desk-signaling';
import {
    SIGNALING_TYPE_CODE_CONFIRM_EXEC,
    SIGNALING_TYPE_CODE_EXEC_CONTROL,
    SIGNALING_TYPE_CODE_EXEC_LIFECYCLE,
    SIGNALING_TYPE_CODE_EXEC_PREVIEW,
    SIGNALING_TYPE_CODE_EXEC_RESULT,
    SIGNALING_TYPE_CODE_EXEC_STATE_REPLY,
    SIGNALING_TYPE_CODE_RESOLVE_EXEC,
} from '../desk/constants';

let nextId = 0;
const sendMessage = vi.fn(() => `req-${++nextId}`);

beforeEach(() => {
    sendMessage.mockClear();
    nextId = 0;
});

const input: ExecRequestInput = {
    shell: 'powershell',
    command: 'Get-Service -Name Spooler',
    cwd: null,
    reason: 'Check the spooler',
};

function previewFrame(requestId: string, preview: ExecPreview): SignalingMessage {
    return {
        request_id: requestId,
        signaling_type: SIGNALING_TYPE_CODE_EXEC_PREVIEW,
        signaling_data: preview,
    };
}

function lifecycleFrame(payload: ExecLifecyclePayload): SignalingMessage {
    return {
        request_id: payload.execution_generation,
        signaling_type: SIGNALING_TYPE_CODE_EXEC_LIFECYCLE,
        signaling_data: payload,
    };
}

function stateReplyFrame(payload: ExecStateReplyPayload): SignalingMessage {
    return {
        request_id: payload.execution_generation,
        signaling_type: SIGNALING_TYPE_CODE_EXEC_STATE_REPLY,
        signaling_data: payload,
    };
}

function resultFrame(payload: ExecResultPayload): SignalingMessage {
    return {
        request_id: 'r-res',
        signaling_type: SIGNALING_TYPE_CODE_EXEC_RESULT,
        signaling_data: payload,
    };
}

function executablePreview(): ExecPreview {
    return {
        exec_request_id: 'exec-1',
        shell: 'powershell',
        command: input.command,
        cwd: null,
        timeout_ms: 30000,
        risk: 'low',
        impact: 'x',
        policy_note: null,
        requires_confirmation: true,
        executable: true,
        blocked_reason: null,
    };
}

function render(orgId?: number) {
    // Controllable signaling subscription: `feed` synchronously delivers a
    // message to the hook's registered handler, mirroring the real lossless
    // fan-out. Call sites already wrap `feed` in `act(...)`.
    const handlers = new Set<(m: SignalingMessage) => void>();
    const subscribe = (h: (m: SignalingMessage) => void) => {
        handlers.add(h);
        return () => {
            handlers.delete(h);
        };
    };
    const props = { deskId: 'desk-1', subscribe, sendMessage, orgId };
    const hook = renderHook((p: typeof props) => useConfirmExec(p), { initialProps: props });
    const feed = (msg: SignalingMessage) => {
        handlers.forEach((h) => h(msg));
    };
    return { hook, feed };
}

describe('useConfirmExec', () => {
    it('sends ConfirmExec and tracks an executable preview', () => {
        const { hook, feed } = render();
        act(() => hook.result.current.requestPreview(0, input));
        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_CONFIRM_EXEC,
            expect.anything(),
            'desk-1',
        );
        expect(hook.result.current.entries[0].phase).toBe('previewing');

        act(() => feed(previewFrame('req-1', executablePreview())));
        expect(hook.result.current.entries[0].phase).toBe('awaiting');
        expect(hook.result.current.entries[0].execRequestId).toBe('exec-1');
    });

    it('forwards the suggestion cwd in the ConfirmExec payload', () => {
        const { hook } = render();
        act(() =>
            hook.result.current.requestPreview(0, { ...input, cwd: '/var/log' }),
        );
        const payload = sendMessage.mock.calls[0][1] as {
            operation: { input: { params: { cwd: string | null } } };
        };
        expect(payload.operation.input.params.cwd).toBe('/var/log');
    });

    it('carries the org hint in the ConfirmExec payload when set, and omits it otherwise', () => {
        // Org view: org_id rides the wire so a non-owner is adjudicated against it.
        const org = render(9);
        act(() => org.hook.result.current.requestPreview(0, input));
        const orgPayload = sendMessage.mock.calls[0][1] as { org_id?: number };
        expect(orgPayload.org_id).toBe(9);

        // Personal view: no org_id, so the open-source server and personal subject
        // behave identically.
        sendMessage.mockClear();
        const personal = render();
        act(() => personal.hook.result.current.requestPreview(0, input));
        const personalPayload = sendMessage.mock.calls[0][1] as { org_id?: number };
        expect(personalPayload.org_id).toBeUndefined();
    });

    it('marks a non-executable (blocked) preview as an error with the reason', () => {
        const { hook, feed } = render();
        act(() => hook.result.current.requestPreview(0, input));
        const preview: ExecPreview = {
            exec_request_id: null,
            shell: 'powershell',
            command: input.command,
            cwd: null,
            timeout_ms: 30000,
            risk: 'blocked',
            impact: 'Blocked: matches a prohibited pattern (download-and-execute)',
            policy_note: null,
            requires_confirmation: false,
            executable: false,
            blocked_reason: 'Blocked: matches a prohibited pattern (download-and-execute)',
        };
        act(() => feed(previewFrame('req-1', preview)));
        expect(hook.result.current.entries[0].phase).toBe('error');
        expect(hook.result.current.entries[0].error).toContain('download-and-execute');
    });

    it('surfaces the policy_note when a preview is non-executable by mode', () => {
        const { hook, feed } = render();
        act(() => hook.result.current.requestPreview(0, input));
        const preview: ExecPreview = {
            exec_request_id: null,
            shell: 'powershell',
            command: input.command,
            cwd: null,
            timeout_ms: 0,
            risk: 'low',
            impact: 'Would read the spooler service status',
            policy_note: 'AI command execution is disabled (suggest-only mode)',
            requires_confirmation: false,
            executable: false,
            blocked_reason: null,
        };
        act(() => feed(previewFrame('req-1', preview)));
        expect(hook.result.current.entries[0].phase).toBe('error');
        expect(hook.result.current.entries[0].error).toContain('suggest-only');
    });

    it('approves and backfills the result by exec_request_id', () => {
        const { hook, feed } = render();
        act(() => hook.result.current.requestPreview(0, input));
        act(() => feed(previewFrame('req-1', executablePreview())));

        act(() => hook.result.current.approve(0));
        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_RESOLVE_EXEC,
            { exec_request_id: 'exec-1', decision: 'approve' },
            'desk-1',
        );
        // Approving is a request, not a report: only the host can say the command
        // started, so the row waits in `dispatching` until it does.
        expect(hook.result.current.entries[0].phase).toBe('dispatching');

        act(() =>
            feed(
                resultFrame({
                    exec_request_id: 'exec-1',
                    outcome: {
                        status: 'ok',
                        data: {
                            kind: 'exec',
                            params: {
                                exit_code: 0,
                                stdout: 'Running',
                                stderr: '',
                                stdout_truncated: false,
                                stderr_truncated: false,
                                duration_ms: 12,
                                redactions: [],
                            },
                        },
                    },
                }),
            ),
        );
        expect(hook.result.current.entries[0].phase).toBe('done');
        expect(hook.result.current.entries[0].output?.exit_code).toBe(0);
        expect(hook.result.current.entries[0].output?.stdout).toBe('Running');
    });

    it('reject sends a reject decision and clears the entry', () => {
        const { hook, feed } = render();
        act(() => hook.result.current.requestPreview(0, input));
        act(() => feed(previewFrame('req-1', executablePreview())));
        act(() => hook.result.current.reject(0));
        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_RESOLVE_EXEC,
            { exec_request_id: 'exec-1', decision: 'reject' },
            'desk-1',
        );
        expect(hook.result.current.entries[0]).toBeUndefined();
    });

    it('surfaces an execution error result', () => {
        const { hook, feed } = render();
        act(() => hook.result.current.requestPreview(0, input));
        act(() => feed(previewFrame('req-1', executablePreview())));
        act(() => hook.result.current.approve(0));
        act(() =>
            feed(
                resultFrame({
                    exec_request_id: 'exec-1',
                    outcome: {
                        status: 'err',
                        data: {
                            kind: 'timeout',
                            message: 'command timed out',
                            retryable: false,
                            safe_for_model: true,
                        },
                    },
                }),
            ),
        );
        expect(hook.result.current.entries[0].phase).toBe('error');
        expect(hook.result.current.entries[0].error).toBe('command timed out');
    });
    /** Approve and return the generation the approving frame was sent under. */
    function approved(hook: ReturnType<typeof render>['hook'], feed: ReturnType<typeof render>['feed']) {
        act(() => hook.result.current.requestPreview(0, input));
        act(() => feed(previewFrame('req-1', executablePreview())));
        act(() => hook.result.current.approve(0));
        const generation = hook.result.current.entries[0].executionGeneration;
        expect(generation).toBeTruthy();
        return generation as string;
    }

    it('only reports a command as running once the host says it started', () => {
        const { hook, feed } = render();
        const generation = approved(hook, feed);
        expect(hook.result.current.entries[0].phase).toBe('dispatching');

        act(() =>
            feed(
                lifecycleFrame({
                    execution_generation: generation,
                    event: 'accepted',
                    containment_identity: 'pgid:4242',
                }),
            ),
        );
        expect(hook.result.current.entries[0].phase).toBe('running');
    });

    it('shows how long the host says a command has been running', () => {
        const { hook, feed } = render();
        const generation = approved(hook, feed);
        act(() =>
            feed(
                lifecycleFrame({
                    execution_generation: generation,
                    event: 'heartbeat',
                    running_ms: 4200,
                }),
            ),
        );
        expect(hook.result.current.entries[0].runningMs).toBe(4200);
    });

    it('ignores a lifecycle frame for a dispatch it is not tracking', () => {
        const { hook, feed } = render();
        approved(hook, feed);
        act(() =>
            feed(
                lifecycleFrame({
                    execution_generation: 'someone-elses-generation',
                    event: 'accepted',
                    containment_identity: null,
                }),
            ),
        );
        expect(hook.result.current.entries[0].phase).toBe('dispatching');
    });

    it('does not reopen a finished row when a late progress frame arrives', () => {
        const { hook, feed } = render();
        const generation = approved(hook, feed);
        act(() =>
            feed(
                resultFrame({
                    exec_request_id: 'exec-1',
                    outcome: {
                        status: 'ok',
                        data: {
                            kind: 'exec',
                            params: {
                                exit_code: 0,
                                stdout: 'ok',
                                stderr: '',
                                stdout_truncated: false,
                                stderr_truncated: false,
                                duration_ms: 1,
                                redactions: [],
                            },
                        },
                    },
                }),
            ),
        );
        expect(hook.result.current.entries[0].phase).toBe('done');

        act(() =>
            feed(
                lifecycleFrame({
                    execution_generation: generation,
                    event: 'heartbeat',
                    running_ms: 9999,
                }),
            ),
        );
        expect(hook.result.current.entries[0].phase).toBe('done');
    });

    it('asks the host to stop a command without claiming it stopped', () => {
        const { hook, feed } = render();
        const generation = approved(hook, feed);
        act(() =>
            feed(
                lifecycleFrame({
                    execution_generation: generation,
                    event: 'accepted',
                    containment_identity: null,
                }),
            ),
        );

        act(() => hook.result.current.cancel(0));
        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_EXEC_CONTROL,
            {
                execution_generation: generation,
                action: 'cancel',
                requested_by: 'control-end',
            },
            'desk-1',
        );
        // Asking is not the same as it having happened: the row stays running and
        // only records that a stop was requested.
        expect(hook.result.current.entries[0].phase).toBe('running');
        expect(hook.result.current.entries[0].cancelRequested).toBe(true);
    });

    it('reports a host that lost track of a command instead of waiting for ever', () => {
        const { hook, feed } = render();
        const generation = approved(hook, feed);
        act(() =>
            feed(
                stateReplyFrame({
                    execution_generation: generation,
                    state: 'indeterminate',
                    containment_identity: null,
                    running_ms: null,
                    detail: 'the host lost track of this execution',
                }),
            ),
        );
        expect(hook.result.current.entries[0].phase).toBe('error');
        expect(hook.result.current.entries[0].error).toContain('lost track');
    });

    it('keeps waiting when a state query says the command is still running', () => {
        const { hook, feed } = render();
        const generation = approved(hook, feed);
        act(() => hook.result.current.queryState(0));
        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_EXEC_CONTROL,
            { execution_generation: generation, action: 'query_state' },
            'desk-1',
        );

        act(() =>
            feed(
                stateReplyFrame({
                    execution_generation: generation,
                    state: 'running',
                    containment_identity: 'pgid:1',
                    running_ms: 1500,
                    detail: null,
                }),
            ),
        );
        expect(hook.result.current.entries[0].phase).toBe('running');
        expect(hook.result.current.entries[0].runningMs).toBe(1500);
    });
});
