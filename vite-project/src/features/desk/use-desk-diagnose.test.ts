import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import {
    useDeskDiagnose,
    extractStreamingSummary,
    buildSnapshotTranscript,
    snapshotConversationKey,
} from './use-desk-diagnose';
import type { DiagnoseEvent, SnapshotMessage } from './use-desk-diagnose';
import type { SignalingMessage } from './use-desk-signaling';
import {
    SIGNALING_TYPE_CODE_DIAGNOSE,
    SIGNALING_TYPE_CODE_DIAGNOSE_EVENT,
    SIGNALING_TYPE_CODE_DIAGNOSE_CANCEL,
    SIGNALING_TYPE_CODE_EXEC_CONTROL,
    SIGNALING_TYPE_CODE_EXEC_PREVIEW,
    SIGNALING_TYPE_CODE_EXEC_STATE_REPLY,
    SIGNALING_TYPE_CODE_RESOLVE_EXEC,
} from './constants';
import type { ExecPreview, ExecStateReplyPayload } from '../exec/use-confirm-exec';

// `sendMessage` returns the wire request_id; the hook keys its aggregation on
// it. Fixed to "req-1" so test frames can target the active request.
const sendMessage = vi.fn(() => 'req-1');

beforeEach(() => {
    sendMessage.mockClear();
    localStorage.clear();
});

afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
});

function frame(event: DiagnoseEvent): SignalingMessage {
    return {
        request_id: event.request_id,
        signaling_type: SIGNALING_TYPE_CODE_DIAGNOSE_EVENT,
        signaling_data: event,
    };
}

// The agentic loop's unsolicited ExecPreview rides the ExecPreview signaling
// type; its wire request_id equals the server-minted exec_request_id.
function execPreviewFrame(overrides: Partial<ExecPreview> = {}): SignalingMessage {
    const preview: ExecPreview = {
        exec_request_id: 'exec-1',
        shell: 'bash',
        command: 'systemctl restart nginx',
        cwd: null,
        timeout_ms: 30_000,
        risk: 'high',
        requires_confirmation: true,
        executable: true,
        blocked_reason: null,
        ...overrides,
    };
    return {
        request_id: preview.exec_request_id ?? 'exec-1',
        signaling_type: SIGNALING_TYPE_CODE_EXEC_PREVIEW,
        signaling_data: preview,
    };
}

describe('extractStreamingSummary', () => {
    it('returns empty before the summary field appears in the JSON', () => {
        expect(extractStreamingSummary('')).toBe('');
        expect(extractStreamingSummary('{"conf')).toBe('');
        expect(extractStreamingSummary('{"confidence":"high"')).toBe('');
    });

    it('streams the partial summary value as the JSON grows', () => {
        expect(extractStreamingSummary('{"summary":"The CPU')).toBe('The CPU');
        expect(extractStreamingSummary('{"summary":"The CPU is high')).toBe('The CPU is high');
    });

    it('stops at the closing quote and ignores later fields', () => {
        expect(
            extractStreamingSummary('{"summary":"done","confidence":"high"}'),
        ).toBe('done');
    });

    it('decodes escape sequences and tolerates a truncated trailing escape', () => {
        expect(extractStreamingSummary('{"summary":"line1\\nline2"}')).toBe('line1\nline2');
        expect(extractStreamingSummary('{"summary":"a \\"quote\\""}')).toBe('a "quote"');
        // Truncated mid-escape at the stream end: emit what is decodable so far.
        expect(extractStreamingSummary('{"summary":"tab\\')).toBe('tab');
        expect(extractStreamingSummary('{"summary":"u\\u00')).toBe('u');
    });

    it('passes through free-text (non-JSON) streams unchanged', () => {
        expect(extractStreamingSummary('The disk is full because')).toBe(
            'The disk is full because',
        );
    });

    it('hides reasoning while a <think> block is still open', () => {
        // DeepSeek-R1 streams its chain-of-thought first; show nothing until the
        // structured answer begins rather than leaking raw reasoning.
        expect(extractStreamingSummary('<think>Let me look at the CPU')).toBe('');
        expect(extractStreamingSummary('<think>still thinking...\nmore')).toBe('');
    });

    it('extracts the summary after a completed <think> block', () => {
        const raw = '<think>reasoning here</think>\n{"summary":"The CPU is busy';
        expect(extractStreamingSummary(raw)).toBe('The CPU is busy');
    });

    it('ignores a ```json fence / prose preamble before the JSON', () => {
        expect(
            extractStreamingSummary('```json\n{"summary":"Disk almost full'),
        ).toBe('Disk almost full');
        expect(
            extractStreamingSummary('Here is my analysis:\n{"summary":"All good'),
        ).toBe('All good');
    });
});

// Controllable signaling subscription: `feed` synchronously delivers a
// DiagnoseEvent frame to the hook's registered handler (wrapped in `act`),
// mirroring the real lossless fan-out that streaming relies on.
function renderDiagnose(deskId: string | null = 'desk-1') {
    const handlers = new Set<(m: SignalingMessage) => void>();
    const subscribe = (h: (m: SignalingMessage) => void) => {
        handlers.add(h);
        return () => {
            handlers.delete(h);
        };
    };
    const view = renderHook(
        ({ deskId }: { deskId: string | null }) =>
            useDeskDiagnose({ deskId, subscribe, sendMessage }),
        { initialProps: { deskId } },
    );
    const feed = (msg: SignalingMessage) =>
        act(() => {
            handlers.forEach((h) => h(msg));
        });
    return { ...view, feed };
}

/**
 * The conversation_id of the Nth Diagnose request, skipping interleaved
 * DiagnoseCancel calls (which carry a null body).
 */
function conversationIdOfCall(n: number): string {
    const diagnoses = sendMessage.mock.calls.filter(
        (c) => c[0] === SIGNALING_TYPE_CODE_DIAGNOSE,
    );
    const body = diagnoses[n][1] as { conversation_id?: string };
    return body.conversation_id as string;
}

describe('useDeskDiagnose', () => {
    it('start sends a Diagnose request and enters the running phase', () => {
        const { result } = renderDiagnose();

        act(() => result.current.start('why slow?', { includeScreen: true }));

        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_DIAGNOSE,
            expect.objectContaining({
                question: 'why slow?',
                include_screen: true,
                context_kinds: [],
                conversation_id: expect.any(String),
            }),
            'desk-1',
        );
        expect(result.current.state.phase).toBe('running');
        expect(result.current.state.requestId).toBe('req-1');
        expect(result.current.state.conversationId).toBe(conversationIdOfCall(0));
    });

    it('aggregates status + partial frames in order, then resolves on final', () => {
        const { result, feed } = renderDiagnose();

        act(() => result.current.start('why?', {}));

        feed(frame({ request_id: 'req-1', seq: 0, kind: 'status', status: 'collecting' }));
        expect(result.current.state.status).toBe('collecting');

        feed(frame({ request_id: 'req-1', seq: 1, kind: 'partial', partial_summary: 'Port ' }));
        feed(frame({ request_id: 'req-1', seq: 2, kind: 'partial', partial_summary: '8080 busy' }));
        expect(result.current.state.partialSummary).toBe('Port 8080 busy');

        feed(
            frame({
                request_id: 'req-1',
                seq: 3,
                kind: 'final',
                final_result: {
                    summary: 'Port conflict',
                    confidence: 'high',
                    findings: [],
                    commands: [],
                    next_steps: [],
                    missing_info: [],
                    collected: ['network.ports'],
                },
            }),
        );
        expect(result.current.state.phase).toBe('done');
        expect(result.current.state.result?.summary).toBe('Port conflict');
        expect(result.current.state.result?.collected).toEqual(['network.ports']);
    });

    it('ignores frames for a different request and stale seq numbers', () => {
        const { result, feed } = renderDiagnose();
        act(() => result.current.start('why?', {}));

        // Wrong request id — ignored.
        feed(frame({ request_id: 'other', seq: 0, kind: 'partial', partial_summary: 'X' }));
        expect(result.current.state.partialSummary).toBe('');

        feed(frame({ request_id: 'req-1', seq: 1, kind: 'partial', partial_summary: 'A' }));
        // Stale seq (<= last applied) — ignored.
        feed(frame({ request_id: 'req-1', seq: 1, kind: 'partial', partial_summary: 'dup' }));
        feed(frame({ request_id: 'req-1', seq: 0, kind: 'partial', partial_summary: 'old' }));
        expect(result.current.state.partialSummary).toBe('A');
    });

    it('an error frame moves to the error phase with the message', () => {
        const { result, feed } = renderDiagnose();
        act(() => result.current.start('why?', {}));
        feed(
            frame({
                request_id: 'req-1',
                seq: 0,
                kind: 'error',
                error: { kind: 'redaction_failed', message: 'redaction failed', retryable: false, safe_for_model: true },
            }),
        );
        expect(result.current.state.phase).toBe('error');
        expect(result.current.state.error).toBe('redaction failed');
    });

    it('reset returns to the idle question form', () => {
        const { result } = renderDiagnose();
        act(() => result.current.start('why?', {}));
        act(() => result.current.reset());
        expect(result.current.state.phase).toBe('idle');
        expect(result.current.state.requestId).toBeNull();
    });

    it('reset while a run is in flight cancels it on the host', () => {
        const { result } = renderDiagnose();
        act(() => result.current.start('why?', {}));
        sendMessage.mockClear();
        act(() => result.current.reset());
        // The in-flight request is cancelled (audited) before we drop tracking.
        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_DIAGNOSE_CANCEL,
            null,
            'desk-1',
            'req-1',
        );
        expect(result.current.state.phase).toBe('idle');
    });

    it('reset from idle does not send a cancel (no in-flight request)', () => {
        const { result } = renderDiagnose();
        act(() => result.current.reset());
        expect(sendMessage).not.toHaveBeenCalled();
    });

    it('tracks the agentic tool timeline and resolves on an answer frame', () => {
        const { result, feed } = renderDiagnose();
        act(() => result.current.start('restart it', {}));

        feed(frame({ request_id: 'req-1', seq: 0, kind: 'turn_started', turn_id: 'turn-1' }));
        expect(result.current.state.turnId).toBe('turn-1');

        feed(
            frame({
                request_id: 'req-1',
                seq: 1,
                kind: 'partial',
                partial_summary: 'let me check',
            }),
        );
        feed(
            frame({
                request_id: 'req-1',
                seq: 2,
                kind: 'tool_started',
                tool_name: 'read_system_info',
                tool_call_id: 'c1',
                tool_arguments_json: '{"detail":true}',
            }),
        );
        expect(result.current.state.timeline).toEqual([
            {
                kind: 'assistant',
                id: 'assistant:req-1:2',
                text: 'let me check',
                provenance: null,
            },
            {
                kind: 'tool',
                id: 'c1',
                activity: {
                    callId: 'c1',
                    name: 'read_system_info',
                    status: 'running',
                    argumentsJson: '{"detail":true}',
                    output: null,
                },
            },
        ]);

        feed(
            frame({
                request_id: 'req-1',
                seq: 3,
                kind: 'tool_finished',
                tool_call_id: 'c1',
                tool_ok: true,
                tool_output: 'hostname=desk-1',
            }),
        );
        expect(result.current.state.timeline[1]).toMatchObject({
            kind: 'tool',
            activity: {
            status: 'ok',
            output: 'hostname=desk-1',
            },
        });

        feed(frame({ request_id: 'req-1', seq: 4, kind: 'answer', answer: 'the host is healthy' }));
        expect(result.current.state.phase).toBe('done');
        expect(result.current.state.timeline[2]).toMatchObject({
            kind: 'assistant',
            text: 'the host is healthy',
        });
        // Frames after the terminal answer are ignored (request closed).
        feed(frame({ request_id: 'req-1', seq: 5, kind: 'answer', answer: 'late' }));
        expect(result.current.state.timeline).toHaveLength(3);
    });

    it('marks a mutating tool as awaiting approval, then failed on a bad finish', () => {
        const { result, feed } = renderDiagnose();
        act(() => result.current.start('do it', {}));

        feed(
            frame({
                request_id: 'req-1',
                seq: 0,
                kind: 'tool_started',
                tool_name: 'exec_command',
                tool_call_id: 'c1',
                awaiting_approval: true,
                tool_arguments_json: '{"command":"restart"}',
            }),
        );
        expect(result.current.state.timeline[0]).toMatchObject({
            kind: 'tool',
            activity: { status: 'awaiting_approval' },
        });

        feed(
            frame({
                request_id: 'req-1',
                seq: 1,
                kind: 'tool_finished',
                tool_call_id: 'c1',
                tool_ok: false,
                tool_output: 'operator rejected the command',
            }),
        );
        expect(result.current.state.timeline[0]).toMatchObject({
            kind: 'tool',
            activity: { status: 'failed' },
        });
    });

    it('lists and restores a resumable historical diagnosis', async () => {
        const summary = {
            sessionId: 'server-session-1',
            conversationId: 'client-conversation-1',
            firstQuestion: 'why slow?',
            createdAt: '2026-07-20T00:00:00Z',
            updatedAt: '2026-07-20T00:01:00Z',
            active: false,
            messageCount: 2,
        };
        const fetchMock = vi.fn(async (input: string | URL | Request) => {
            const url = String(input);
            return {
                ok: true,
                json: async () =>
                    url.includes('/diagnose-sessions?')
                        ? {
                              success: true,
                              code: 0,
                              data: { sessions: [summary] },
                          }
                        : {
                              success: true,
                              code: 0,
                              data: {
                                  seq: 4,
                                  active: false,
                                  requestId: 'req-old',
                                  messages: [
                                      { id: 'u1', role: 'user', text: 'why slow?' },
                                      {
                                          id: 'a1',
                                          role: 'assistant',
                                          text: 'A process is using the CPU.',
                                      },
                                  ],
                              },
                          },
            } as Response;
        });
        vi.stubGlobal('fetch', fetchMock);
        const { result } = renderDiagnose();

        await act(async () => result.current.refreshHistory());
        expect(result.current.historySessions).toEqual([summary]);

        await act(async () => result.current.restoreSession(summary));
        expect(result.current.state.phase).toBe('done');
        expect(result.current.state.history[0].question).toBe('why slow?');
        expect(result.current.canContinue).toBe(true);
        expect(localStorage.getItem(snapshotConversationKey('desk-1'))).toBe(
            'client-conversation-1',
        );
        expect(fetchMock).toHaveBeenLastCalledWith(
            expect.stringContaining('&session=server-session-1'),
            expect.any(Object),
        );
    });

    it('restores a legacy historical diagnosis as read-only', async () => {
        const fetchMock = vi.fn().mockResolvedValue({
            ok: true,
            json: async () => ({
                success: true,
                code: 0,
                data: {
                    seq: 2,
                    active: false,
                    messages: [
                        { id: 'u1', role: 'user', text: 'legacy question' },
                        { id: 'a1', role: 'assistant', text: 'legacy answer' },
                    ],
                },
            }),
        });
        vi.stubGlobal('fetch', fetchMock);
        const { result } = renderDiagnose();
        await act(async () =>
            result.current.restoreSession({
                sessionId: 'legacy-session',
                conversationId: null,
                firstQuestion: 'legacy question',
                createdAt: '2026-07-01T00:00:00Z',
                updatedAt: '2026-07-01T00:01:00Z',
                active: false,
                messageCount: 2,
            }),
        );

        expect(result.current.state.phase).toBe('done');
        expect(result.current.canContinue).toBe(false);
        act(() => result.current.start('try to continue', {}));
        expect(sendMessage).not.toHaveBeenCalled();
    });

    it('captures an agentic ExecPreview while a run is in flight and approves it', () => {
        const { result, feed } = renderDiagnose();
        act(() => result.current.start('restart nginx', {}));

        feed(
            frame({
                request_id: 'req-1',
                seq: 0,
                kind: 'tool_started',
                tool_name: 'exec_command',
                tool_call_id: 'c1',
                awaiting_approval: true,
                tool_arguments_json: '{"command":"systemctl restart nginx"}',
            }),
        );
        feed(execPreviewFrame());
        expect(result.current.state.pendingExec?.command).toBe('systemctl restart nginx');
        expect(result.current.state.timeline[0]).toMatchObject({
            kind: 'tool',
            activity: { status: 'awaiting_approval' },
        });

        sendMessage.mockClear();
        act(() => result.current.approveExec());
        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_RESOLVE_EXEC,
            { exec_request_id: 'exec-1', decision: 'approve' },
            'desk-1',
        );
        // The card clears once resolved; completion shows via the tool timeline.
        expect(result.current.state.pendingExec).toBeNull();
        expect(result.current.state.timeline[0]).toMatchObject({
            kind: 'tool',
            activity: { status: 'running' },
        });
    });

    it('recovers a settled live request from the persisted snapshot', async () => {
        vi.useFakeTimers();
        const fetchMock = vi.fn().mockResolvedValue({
            status: 200,
            ok: true,
            json: async () => ({
                success: true,
                code: 0,
                data: {
                    seq: 9,
                    active: false,
                    requestId: 'req-1',
                    activeExecutionGeneration: 'generation-bg-1',
                    messages: [
                        { id: 'u1', role: 'user', text: 'run it' },
                        {
                            id: 'a1',
                            role: 'assistant',
                            text: '',
                            toolCalls: [
                                {
                                    id: 'c1',
                                    name: 'exec_command',
                                    argumentsJson: '{"command":"Start-Sleep -Seconds 30"}',
                                },
                            ],
                        },
                        {
                            id: 't1',
                            role: 'tool',
                            text: 'the approval session was cancelled',
                            toolCallId: 'c1',
                        },
                        { id: 'a2', role: 'assistant', text: 'The command did not run.' },
                    ],
                },
            }),
        });
        vi.stubGlobal('fetch', fetchMock);
        Object.defineProperty(document, 'visibilityState', {
            configurable: true,
            value: 'visible',
        });

        const { result, feed } = renderDiagnose();
        act(() => result.current.start('run it', {}));
        await act(async () => {
            document.dispatchEvent(new Event('visibilitychange'));
            await Promise.resolve();
            await Promise.resolve();
        });

        expect(result.current.state.phase).toBe('done');
        expect(result.current.state.history).toHaveLength(1);
        expect(result.current.state.history[0].timeline[1]).toMatchObject({
            kind: 'assistant',
            text: 'The command did not run.',
        });
        expect(result.current.state.history[0].timeline[0]).toMatchObject({
            kind: 'tool',
            activity: {
                status: 'ok',
                output: 'the approval session was cancelled',
            },
        });
        expect(result.current.state.backgroundExecution).toEqual({
            executionGeneration: 'generation-bg-1',
            cancelRequested: false,
        });

        sendMessage.mockClear();
        act(() => result.current.cancelBackgroundExec());
        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_EXEC_CONTROL,
            {
                execution_generation: 'generation-bg-1',
                action: 'cancel',
                requested_by: 'diagnose-operator',
            },
            'desk-1',
        );
        expect(result.current.state.backgroundExecution?.cancelRequested).toBe(true);

        sendMessage.mockClear();
        act(() => vi.advanceTimersByTime(500));
        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_EXEC_CONTROL,
            {
                execution_generation: 'generation-bg-1',
                action: 'query_state',
            },
            'desk-1',
        );

        const running: ExecStateReplyPayload = {
            execution_generation: 'generation-bg-1',
            state: 'running',
            containment_identity: null,
            running_ms: 1_000,
            detail: null,
        };
        feed({
            request_id: 'state-running',
            signaling_type: SIGNALING_TYPE_CODE_EXEC_STATE_REPLY,
            signaling_data: running,
        });
        expect(result.current.state.backgroundExecution?.cancelRequested).toBe(true);

        const terminal: ExecStateReplyPayload = {
            ...running,
            state: 'terminal',
            running_ms: null,
        };
        feed({
            request_id: 'state-terminal',
            signaling_type: SIGNALING_TYPE_CODE_EXEC_STATE_REPLY,
            signaling_data: terminal,
        });
        expect(result.current.state.backgroundExecution).toBeNull();

        sendMessage.mockClear();
        act(() => vi.advanceTimersByTime(1_000));
        expect(sendMessage).not.toHaveBeenCalled();
    });

    it('rejects an agentic ExecPreview with a reject decision', () => {
        const { result, feed } = renderDiagnose();
        act(() => result.current.start('restart nginx', {}));
        feed(execPreviewFrame());

        sendMessage.mockClear();
        act(() => result.current.rejectExec());
        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_RESOLVE_EXEC,
            { exec_request_id: 'exec-1', decision: 'reject' },
            'desk-1',
        );
        expect(result.current.state.pendingExec).toBeNull();
    });

    it('ignores an ExecPreview when no run is in flight (suggested-command path owns it)', () => {
        const { result, feed } = renderDiagnose();
        // No start() — activeRequest is null, so the preview is not the agentic one.
        feed(execPreviewFrame());
        expect(result.current.state.pendingExec).toBeNull();
    });

    it('clears a pending approval when the run terminates', () => {
        const { result, feed } = renderDiagnose();
        act(() => result.current.start('restart nginx', {}));
        feed(execPreviewFrame());
        expect(result.current.state.pendingExec).not.toBeNull();

        feed(frame({ request_id: 'req-1', seq: 0, kind: 'answer', answer: 'done' }));
        expect(result.current.state.phase).toBe('done');
        expect(result.current.state.pendingExec).toBeNull();
    });

    it('threads a stable conversation_id across follow-up turns and keeps history', () => {
        const { result, feed } = renderDiagnose();
        act(() => result.current.start('why slow?', {}));
        const conv = conversationIdOfCall(0);
        expect(conv).toBeTruthy();
        expect(result.current.state.conversationId).toBe(conv);

        // Finish the first turn.
        feed(frame({ request_id: 'req-1', seq: 0, kind: 'answer', answer: 'cpu is busy' }));
        expect(result.current.state.phase).toBe('done');

        // Follow up: same conversation id, prior turn snapshotted into history.
        act(() => result.current.start('and memory?', {}));
        expect(conversationIdOfCall(1)).toBe(conv);
        expect(result.current.state.phase).toBe('running');
        expect(result.current.state.question).toBe('and memory?');
        expect(result.current.state.history).toHaveLength(1);
        expect(result.current.state.history[0].question).toBe('why slow?');
        expect(result.current.state.history[0].timeline[0]).toMatchObject({
            kind: 'assistant',
            text: 'cpu is busy',
        });
    });

    it('a follow-up after a circuit breaker keeps prior output and continues the same conversation', () => {
        const { result, feed } = renderDiagnose();
        act(() => result.current.start('why slow?', {}));
        const conv = conversationIdOfCall(0);
        feed(
            frame({
                request_id: 'req-1',
                seq: 0,
                kind: 'partial',
                partial_summary: 'process 4242 is using the CPU',
            }),
        );
        feed(
            frame({
                request_id: 'req-1',
                seq: 1,
                kind: 'tool_started',
                tool_name: 'execute_command',
                tool_call_id: 'c1',
                tool_arguments_json: '{"command":"ps"}',
            }),
        );
        feed(
            frame({
                request_id: 'req-1',
                seq: 2,
                kind: 'tool_finished',
                tool_call_id: 'c1',
                tool_ok: true,
                tool_output: 'pid=4242',
            }),
        );
        feed(
            frame({
                request_id: 'req-1',
                seq: 3,
                kind: 'error',
                error: {
                    kind: 'internal',
                    message: 'repeat limit',
                    retryable: false,
                    safe_for_model: true,
                    error_code: 70,
                },
            }),
        );
        expect(result.current.state.phase).toBe('error');
        expect(result.current.state.partialSummary).toBe('');
        expect(result.current.state.timeline).toEqual([
            {
                kind: 'assistant',
                id: 'assistant:req-1:1',
                text: 'process 4242 is using the CPU',
                provenance: null,
            },
            {
                kind: 'tool',
                id: 'c1',
                activity: {
                    callId: 'c1',
                    name: 'execute_command',
                    status: 'ok',
                    argumentsJson: '{"command":"ps"}',
                    output: 'pid=4242',
                },
            },
        ]);

        act(() => result.current.start('continue', {}));
        // The failed turn is settled, so the follow-up reuses the conversation and
        // the failed turn is captured in the transcript.
        expect(conversationIdOfCall(1)).toBe(conv);
        expect(result.current.state.history).toHaveLength(1);
        expect(result.current.state.history[0].phase).toBe('error');
        expect(result.current.state.history[0].summary).toBe('');
        expect(result.current.state.history[0].timeline).toEqual([
            {
                kind: 'assistant',
                id: 'assistant:req-1:1',
                text: 'process 4242 is using the CPU',
                provenance: null,
            },
            {
                kind: 'tool',
                id: 'c1',
                activity: {
                    callId: 'c1',
                    name: 'execute_command',
                    status: 'ok',
                    argumentsJson: '{"command":"ps"}',
                    output: 'pid=4242',
                },
            },
        ]);
        expect(result.current.state.history[0].error).toBe('repeat limit');
        expect(result.current.state.history[0].errorCode).toBe(70);
    });

    it('reset starts a new conversation on the next turn', () => {
        const { result } = renderDiagnose();
        act(() => result.current.start('q1', {}));
        act(() => result.current.reset());
        act(() => result.current.start('q2', {}));
        expect(conversationIdOfCall(1)).not.toBe(conversationIdOfCall(0));
        expect(result.current.state.history).toEqual([]);
    });

    it('a desk change regenerates the conversation and clears the transcript', () => {
        const { result, feed, rerender } = renderDiagnose('desk-1');
        act(() => result.current.start('q1', {}));
        feed(frame({ request_id: 'req-1', seq: 0, kind: 'answer', answer: 'a' }));
        act(() => result.current.start('q2', {}));
        expect(result.current.state.history).toHaveLength(1);

        act(() => rerender({ deskId: 'desk-2' }));
        expect(result.current.state.phase).toBe('idle');
        expect(result.current.state.history).toEqual([]);
        expect(result.current.state.conversationId).toBeNull();

        act(() => result.current.start('q3', {}));
        // q1, q2, q3 are the three Diagnose requests; q3 must open a new conversation.
        expect(conversationIdOfCall(2)).not.toBe(conversationIdOfCall(0));
    });
});

describe('snapshotConversationKey', () => {
    it('namespaces the persisted conversation intent by desk', () => {
        expect(snapshotConversationKey('desk-1')).toBe('lrd:diagnose-conv:desk-1');
        expect(snapshotConversationKey('desk-1')).not.toBe(snapshotConversationKey('desk-2'));
    });
});

describe('buildSnapshotTranscript', () => {
    it('is empty for no messages', () => {
        expect(buildSnapshotTranscript([])).toEqual([]);
    });

    it('groups messages into turns at each user message', () => {
        const messages: SnapshotMessage[] = [
            { id: 'u1', role: 'user', text: 'why is cpu high?' },
            { id: 'a1', role: 'assistant', text: 'A runaway process.' },
            { id: 'u2', role: 'user', text: 'and memory?' },
            { id: 'a2', role: 'assistant', text: 'Memory is fine.' },
        ];
        const turns = buildSnapshotTranscript(messages);
        expect(turns).toHaveLength(2);
        expect(turns[0].question).toBe('why is cpu high?');
        expect(turns[0].timeline[0]).toMatchObject({
            kind: 'assistant',
            text: 'A runaway process.',
        });
        expect(turns[0].requestId).toBe('u1');
        expect(turns[0].phase).toBe('done');
        expect(turns[1].question).toBe('and memory?');
        expect(turns[1].timeline[0]).toMatchObject({
            kind: 'assistant',
            text: 'Memory is fine.',
        });
    });

    it('appends background completion between the command result and automation follow-up', () => {
        const messages: SnapshotMessage[] = [
            { id: 'u1', role: 'user', text: 'restart the service' },
            {
                id: 'a1',
                role: 'assistant',
                text: 'I will restart it.',
                toolCalls: [
                    {
                        id: 'c1',
                        name: 'exec_command',
                        argumentsJson: '{"command":"restart"}',
                    },
                ],
            },
            {
                id: 't1',
                role: 'tool',
                text: 'command dispatched as background task',
                toolCallId: 'c1',
            },
            {
                id: 'out1',
                role: 'untrusted_output',
                text: 'exit_code=0',
                toolCallId: 'c1',
            },
            { id: 'a2', role: 'assistant', text: 'The restart succeeded (exit 0).' },
        ];
        const turns = buildSnapshotTranscript(messages);
        expect(turns).toHaveLength(1);
        expect(turns[0].timeline.map((item) => item.kind)).toEqual([
            'assistant',
            'tool',
            'background_completion',
            'assistant',
        ]);
        expect(turns[0].timeline[1]).toMatchObject({
            kind: 'tool',
            activity: { output: 'command dispatched as background task' },
        });
        expect(turns[0].timeline[2]).toEqual({
            kind: 'background_completion',
            id: 'out1',
            toolCallId: 'c1',
            output: 'exit_code=0',
        });
    });

    it('captures assistant tool calls as tool activity and skips internal messages', () => {
        const messages: SnapshotMessage[] = [
            { id: 'u1', role: 'user', text: 'check disk' },
            {
                id: 'a1',
                role: 'assistant',
                text: '',
                toolCalls: [{ id: 'c1', name: 'read_disk', argumentsJson: '{}' }],
            },
            { id: 't1', role: 'tool', text: '90% full', toolCallId: 'c1' },
            { id: 'a2', role: 'assistant', text: 'The disk is 90% full.' },
        ];
        const turns = buildSnapshotTranscript(messages);
        expect(turns).toHaveLength(1);
        expect(turns[0].timeline).toEqual([
            {
                kind: 'tool',
                id: 'c1',
                activity: {
                    callId: 'c1',
                    name: 'read_disk',
                    status: 'ok',
                    argumentsJson: '{}',
                    output: '90% full',
                },
            },
            {
                kind: 'assistant',
                id: 'a2',
                text: 'The disk is 90% full.',
                provenance: null,
            },
        ]);
    });

    it('tolerates a leading assistant message with no preceding user turn', () => {
        const messages: SnapshotMessage[] = [
            { id: 'a0', role: 'assistant', text: 'orphan answer' },
        ];
        const turns = buildSnapshotTranscript(messages);
        expect(turns).toHaveLength(1);
        expect(turns[0].question).toBe('');
        expect(turns[0].timeline[0]).toMatchObject({
            kind: 'assistant',
            text: 'orphan answer',
        });
    });
});
