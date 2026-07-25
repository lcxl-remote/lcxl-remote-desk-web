import { describe, it, expect, vi, beforeEach } from 'vitest';
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
    SIGNALING_TYPE_CODE_EXEC_PREVIEW,
    SIGNALING_TYPE_CODE_RESOLVE_EXEC,
} from './constants';
import type { ExecPreview } from '../exec/use-confirm-exec';

// `sendMessage` returns the wire request_id; the hook keys its aggregation on
// it. Fixed to "req-1" so test frames can target the active request.
const sendMessage = vi.fn(() => 'req-1');

beforeEach(() => {
    sendMessage.mockClear();
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
        impact: 'Restarts the nginx service.',
        policy_note: null,
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

    it('handoff sends DiagnoseCancel with the diagnosis id and keeps the result', () => {
        const { result, feed } = renderDiagnose();
        act(() => result.current.start('why?', {}));
        feed(frame({ request_id: 'req-1', seq: 0, kind: 'partial', partial_summary: 'partial text' }));

        act(() => result.current.handoff());

        expect(sendMessage).toHaveBeenLastCalledWith(
            SIGNALING_TYPE_CODE_DIAGNOSE_CANCEL,
            null,
            'desk-1',
            'req-1',
        );
        expect(result.current.state.phase).toBe('done');
        expect(result.current.state.partialSummary).toBe('partial text');

        // Frames after handoff are ignored (no active request).
        feed(frame({ request_id: 'req-1', seq: 1, kind: 'partial', partial_summary: ' more' }));
        expect(result.current.state.partialSummary).toBe('partial text');
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
                kind: 'tool_started',
                tool_name: 'read_system_info',
                tool_call_id: 'c1',
            }),
        );
        expect(result.current.state.tools).toEqual([
            { callId: 'c1', name: 'read_system_info', status: 'running' },
        ]);

        feed(frame({ request_id: 'req-1', seq: 2, kind: 'tool_finished', tool_call_id: 'c1', tool_ok: true }));
        expect(result.current.state.tools[0].status).toBe('ok');

        feed(frame({ request_id: 'req-1', seq: 3, kind: 'answer', answer: 'the host is healthy' }));
        expect(result.current.state.phase).toBe('done');
        expect(result.current.state.answer).toBe('the host is healthy');
        // Frames after the terminal answer are ignored (request closed).
        feed(frame({ request_id: 'req-1', seq: 4, kind: 'answer', answer: 'late' }));
        expect(result.current.state.answer).toBe('the host is healthy');
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
            }),
        );
        expect(result.current.state.tools[0].status).toBe('awaiting_approval');

        feed(frame({ request_id: 'req-1', seq: 1, kind: 'tool_finished', tool_call_id: 'c1', tool_ok: false }));
        expect(result.current.state.tools[0].status).toBe('failed');
    });

    it('captures an agentic ExecPreview while a run is in flight and approves it', () => {
        const { result, feed } = renderDiagnose();
        act(() => result.current.start('restart nginx', {}));

        feed(execPreviewFrame());
        expect(result.current.state.pendingExec?.command).toBe('systemctl restart nginx');

        sendMessage.mockClear();
        act(() => result.current.approveExec());
        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_RESOLVE_EXEC,
            { exec_request_id: 'exec-1', decision: 'approve' },
            'desk-1',
        );
        // The card clears once resolved; completion shows via the tool timeline.
        expect(result.current.state.pendingExec).toBeNull();
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
        expect(result.current.state.history[0].answer).toBe('cpu is busy');
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
            }),
        );
        feed(
            frame({
                request_id: 'req-1',
                seq: 2,
                kind: 'tool_finished',
                tool_call_id: 'c1',
                tool_ok: true,
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
        expect(result.current.state.partialSummary).toBe('process 4242 is using the CPU');
        expect(result.current.state.tools).toEqual([
            { callId: 'c1', name: 'execute_command', status: 'ok' },
        ]);

        act(() => result.current.start('continue', {}));
        // The failed turn is settled, so the follow-up reuses the conversation and
        // the failed turn is captured in the transcript.
        expect(conversationIdOfCall(1)).toBe(conv);
        expect(result.current.state.history).toHaveLength(1);
        expect(result.current.state.history[0].phase).toBe('error');
        expect(result.current.state.history[0].summary).toBe('process 4242 is using the CPU');
        expect(result.current.state.history[0].tools).toEqual([
            { callId: 'c1', name: 'execute_command', status: 'ok' },
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

    it('handoff makes the next turn a new conversation', () => {
        const { result, feed } = renderDiagnose();
        act(() => result.current.start('q1', {}));
        feed(frame({ request_id: 'req-1', seq: 0, kind: 'answer', answer: 'a' }));
        act(() => result.current.handoff());
        act(() => result.current.start('q2', {}));
        expect(conversationIdOfCall(1)).not.toBe(conversationIdOfCall(0));
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
        expect(turns[0].answer).toBe('A runaway process.');
        expect(turns[0].requestId).toBe('u1');
        expect(turns[0].phase).toBe('done');
        expect(turns[1].question).toBe('and memory?');
        expect(turns[1].answer).toBe('Memory is fine.');
    });

    it('joins several assistant answers in one turn — an automation follow-up appears appended', () => {
        // A user turn, its original answer, the command output the automation turn
        // reacted to (untrusted_output, skipped from the visible text), then the
        // automation answer — all before any new user message, so one turn.
        const messages: SnapshotMessage[] = [
            { id: 'u1', role: 'user', text: 'restart the service' },
            { id: 'a1', role: 'assistant', text: 'Dispatched; it is restarting.' },
            { id: 'out1', role: 'untrusted_output', text: 'exit_code=0' },
            { id: 'a2', role: 'assistant', text: 'The restart succeeded (exit 0).' },
        ];
        const turns = buildSnapshotTranscript(messages);
        expect(turns).toHaveLength(1);
        expect(turns[0].answer).toBe(
            'Dispatched; it is restarting.\n\nThe restart succeeded (exit 0).',
        );
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
        expect(turns[0].tools).toEqual([{ callId: 'c1', name: 'read_disk', status: 'ok' }]);
        expect(turns[0].answer).toBe('The disk is 90% full.');
    });

    it('tolerates a leading assistant message with no preceding user turn', () => {
        const messages: SnapshotMessage[] = [
            { id: 'a0', role: 'assistant', text: 'orphan answer' },
        ];
        const turns = buildSnapshotTranscript(messages);
        expect(turns).toHaveLength(1);
        expect(turns[0].question).toBe('');
        expect(turns[0].answer).toBe('orphan answer');
    });
});
