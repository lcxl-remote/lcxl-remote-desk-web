import { describe, it, expect, vi, beforeEach } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useDeskExec } from './use-desk-exec';
import type { ExecPreview, ExecResultPayload } from './use-desk-exec';
import type { SuggestedCommand } from './use-desk-diagnose';
import type { SignalingMessage } from './use-desk-signaling';
import {
    SIGNALING_TYPE_CODE_CONFIRM_EXEC,
    SIGNALING_TYPE_CODE_EXEC_PREVIEW,
    SIGNALING_TYPE_CODE_EXEC_RESULT,
    SIGNALING_TYPE_CODE_RESOLVE_EXEC,
} from './constants';

let nextId = 0;
const sendMessage = vi.fn(() => `req-${++nextId}`);

beforeEach(() => {
    sendMessage.mockClear();
    nextId = 0;
});

const command: SuggestedCommand = {
    shell: 'powershell',
    command: 'Get-Service -Name Spooler',
    purpose: 'Check the spooler',
    risk: 'low',
    requires_confirmation: true,
};

function previewFrame(requestId: string, preview: ExecPreview): SignalingMessage {
    return {
        request_id: requestId,
        signaling_type: SIGNALING_TYPE_CODE_EXEC_PREVIEW,
        signaling_data: preview,
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
        command: command.command,
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

function render() {
    const props = { deskId: 'desk-1', lastMessage: null as SignalingMessage | null, sendMessage };
    const hook = renderHook((p: typeof props) => useDeskExec(p), { initialProps: props });
    const feed = (msg: SignalingMessage) => hook.rerender({ ...props, lastMessage: msg });
    return { hook, feed };
}

describe('useDeskExec', () => {
    it('sends ConfirmExec and tracks an executable preview', () => {
        const { hook, feed } = render();
        act(() => hook.result.current.requestPreview(0, command));
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

    it('marks a non-executable (blocked) preview as an error with the reason', () => {
        const { hook, feed } = render();
        act(() => hook.result.current.requestPreview(0, command));
        const preview: ExecPreview = {
            exec_request_id: null,
            shell: 'powershell',
            command: command.command,
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

    it('approves and backfills the result by exec_request_id', () => {
        const { hook, feed } = render();
        act(() => hook.result.current.requestPreview(0, command));
        act(() => feed(previewFrame('req-1', executablePreview())));

        act(() => hook.result.current.approve(0));
        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_RESOLVE_EXEC,
            { exec_request_id: 'exec-1', decision: 'approve' },
            'desk-1',
        );
        expect(hook.result.current.entries[0].phase).toBe('running');

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
        act(() => hook.result.current.requestPreview(0, command));
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
        act(() => hook.result.current.requestPreview(0, command));
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
});
