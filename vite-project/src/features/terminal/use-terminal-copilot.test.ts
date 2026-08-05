import { describe, it, expect, vi, beforeEach } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useTerminalCopilot } from './use-terminal-copilot';
import type { TerminalCopilotEvent, TerminalContext } from './use-terminal-copilot';
import type { SignalingMessage } from '../desk/use-desk-signaling';
import {
    SIGNALING_TYPE_CODE_TERMINAL_COPILOT_ASK,
    SIGNALING_TYPE_CODE_TERMINAL_COPILOT_EVENT,
    SIGNALING_TYPE_CODE_TERMINAL_COPILOT_CANCEL,
} from '../desk/constants';

// `sendMessage` returns the wire request_id the hook keys aggregation on.
const sendMessage = vi.fn(() => 'req-1');

beforeEach(() => {
    sendMessage.mockClear();
});

function eventFrame(event: TerminalCopilotEvent): SignalingMessage {
    return {
        request_id: event.request_id,
        signaling_type: SIGNALING_TYPE_CODE_TERMINAL_COPILOT_EVENT,
        signaling_data: event,
    };
}

const ctx: TerminalContext = {
    os: 'linux',
    shell: 'bash',
    recent_output: 'bind: address already in use',
};

// Controllable signaling subscription: `feed` synchronously delivers a frame to
// the hook's registered handler (wrapped in `act`), mirroring the real lossless
// fan-out streaming relies on.
function renderCopilot(connectionId: string | null = 'conn-1') {
    const handlers = new Set<(m: SignalingMessage) => void>();
    const subscribe = (h: (m: SignalingMessage) => void) => {
        handlers.add(h);
        return () => {
            handlers.delete(h);
        };
    };
    const view = renderHook(() =>
        useTerminalCopilot({ connectionId, subscribe, sendMessage }),
    );
    const feed = (msg: SignalingMessage) =>
        act(() => {
            handlers.forEach((h) => h(msg));
        });
    return { ...view, feed };
}

describe('useTerminalCopilot', () => {
    it('ask sends a TerminalCopilotAsk to the target and enters running', () => {
        const { result } = renderCopilot();
        act(() => result.current.ask({ mode: 'how_to', question: 'free port 8080', context: ctx }));

        expect(sendMessage).toHaveBeenCalledTimes(1);
        const [type, data, connectionId] = sendMessage.mock.calls[0];
        expect(type).toBe(SIGNALING_TYPE_CODE_TERMINAL_COPILOT_ASK);
        // The target rides the outer to_connection_id, not the payload.
        expect(connectionId).toBe('conn-1');
        expect((data as { mode: string }).mode).toBe('how_to');
        expect((data as { conversation_id?: string }).conversation_id).toBeTruthy();
        expect(result.current.state.phase).toBe('running');
    });

    it('aggregates tool_started then final into a structured answer', () => {
        const { result, feed } = renderCopilot();
        act(() => result.current.ask({ mode: 'how_to', question: 'q', context: ctx }));

        feed(eventFrame({ request_id: 'req-1', seq: 0, kind: 'tool_started', tool_name: 'read_process_list' }));
        feed(
            eventFrame({
                request_id: 'req-1',
                seq: 1,
                kind: 'final',
                answer: {
                    explanation_md: 'Port 8080 is held by a process.',
                    suggestions: [
                        {
                            command: 'lsof -i :8080',
                            shell: 'bash',
                            note: 'List the listener.',
                            risk: 'low',
                            decision: 'not_executable',
                        },
                    ],
                },
            }),
        );

        expect(result.current.state.tools).toEqual([{ name: 'read_process_list' }]);
        expect(result.current.state.phase).toBe('done');
        const turns = result.current.state.turns;
        expect(turns).toHaveLength(1);
        expect(turns[0].answer?.suggestions[0].decision).toBe('not_executable');
    });

    it('replays prior completed turns as history on a follow-up ask', () => {
        const { result, feed } = renderCopilot();
        act(() => result.current.ask({ mode: 'how_to', question: 'free port 8080', context: ctx }));
        feed(
            eventFrame({
                request_id: 'req-1',
                seq: 0,
                kind: 'final',
                answer: { explanation_md: 'Port 8080 is held by nginx.', suggestions: [] },
            }),
        );

        // The follow-up carries the prior exchange so the stateless brain can
        // continue the thread; the in-flight turn is not part of the sent history.
        act(() => result.current.ask({ mode: 'how_to', question: 'now stop it', context: ctx }));
        const asks = sendMessage.mock.calls.filter(
            (c) => c[0] === SIGNALING_TYPE_CODE_TERMINAL_COPILOT_ASK,
        );
        const second = asks[1][1] as { history: { user: string; assistant: string }[] };
        expect(second.history).toEqual([
            { user: 'free port 8080', assistant: 'Port 8080 is held by nginx.' },
        ]);
        // Both turns are present in the conversation.
        expect(result.current.state.turns).toHaveLength(2);
    });

    it('ignores stale / out-of-order frames by seq', () => {
        const { result, feed } = renderCopilot();
        act(() => result.current.ask({ mode: 'how_to', question: 'q', context: ctx }));

        feed(eventFrame({ request_id: 'req-1', seq: 0, kind: 'partial', partial_text: 'first ' }));
        // A replayed seq 0 must not double-append.
        feed(eventFrame({ request_id: 'req-1', seq: 0, kind: 'partial', partial_text: 'dupe ' }));
        feed(eventFrame({ request_id: 'req-1', seq: 1, kind: 'partial', partial_text: 'second' }));

        expect(result.current.state.partialText).toBe('first second');
    });

    it('keeps reviewed text visible and retracts unsafe provisional text', () => {
        const { result, feed } = renderCopilot();
        act(() => result.current.ask({ mode: 'how_to', question: 'q', context: ctx }));

        feed(
            eventFrame({
                request_id: 'req-1',
                seq: 0,
                kind: 'partial',
                partial_text: 'reviewed ',
            }),
        );
        feed(eventFrame({ request_id: 'req-1', seq: 1, kind: 'partial_committed' }));
        expect(result.current.state.partialText).toBe('');
        expect(result.current.state.committedText).toBe('reviewed ');

        feed(
            eventFrame({
                request_id: 'req-1',
                seq: 2,
                kind: 'partial',
                partial_text: 'unsafe',
            }),
        );
        feed(
            eventFrame({
                request_id: 'req-1',
                seq: 3,
                kind: 'retracted',
                retraction_reason: 'safe_redirect',
                error: {
                    kind: 'content_blocked',
                    message: 'provider text must not be displayed',
                    retryable: false,
                    safe_for_model: true,
                },
            }),
        );

        expect(result.current.state.phase).toBe('error');
        expect(result.current.state.partialText).toBe('');
        expect(result.current.state.committedText).toBe('reviewed ');
        expect(result.current.state.retractionReason).toBe('safe_redirect');
        feed(
            eventFrame({
                request_id: 'req-1',
                seq: 4,
                kind: 'partial',
                partial_text: 'late',
            }),
        );
        expect(result.current.state.partialText).toBe('');
    });

    it('drops frames whose request_id does not match the active ask', () => {
        const { result, feed } = renderCopilot();
        act(() => result.current.ask({ mode: 'how_to', question: 'q', context: ctx }));

        feed(eventFrame({ request_id: 'other', seq: 0, kind: 'error', error: { kind: 'Internal', message: 'nope', retryable: false, safe_for_model: true } }));
        expect(result.current.state.phase).toBe('running');
    });

    it('surfaces a terminal error frame', () => {
        const { result, feed } = renderCopilot();
        act(() => result.current.ask({ mode: 'explain_error', context: ctx }));

        feed(
            eventFrame({
                request_id: 'req-1',
                seq: 0,
                kind: 'error',
                error: { kind: 'RedactionFailed', message: 'failed to redact', retryable: false, safe_for_model: true },
            }),
        );
        expect(result.current.state.phase).toBe('error');
        expect(result.current.state.error).toBe('failed to redact');
    });

    it('reset sends a cancel for an in-flight ask and clears state', () => {
        const { result } = renderCopilot();
        act(() => result.current.ask({ mode: 'how_to', question: 'q', context: ctx }));
        act(() => result.current.reset());

        const cancel = sendMessage.mock.calls.find(
            (c) => c[0] === SIGNALING_TYPE_CODE_TERMINAL_COPILOT_CANCEL,
        );
        expect(cancel).toBeTruthy();
        // The cancel correlates by the original request id and targets the host.
        expect(cancel?.[2]).toBe('conn-1');
        expect(cancel?.[3]).toBe('req-1');
        expect(result.current.state.phase).toBe('idle');
    });

    it('threads one conversation id across follow-up asks until reset', () => {
        const { result } = renderCopilot();
        act(() => result.current.ask({ mode: 'how_to', question: 'a', context: ctx }));
        act(() => result.current.ask({ mode: 'how_to', question: 'b', context: ctx }));

        const asks = sendMessage.mock.calls.filter(
            (c) => c[0] === SIGNALING_TYPE_CODE_TERMINAL_COPILOT_ASK,
        );
        const first = (asks[0][1] as { conversation_id: string }).conversation_id;
        const second = (asks[1][1] as { conversation_id: string }).conversation_id;
        expect(first).toBe(second);

        act(() => result.current.reset());
        act(() => result.current.ask({ mode: 'how_to', question: 'c', context: ctx }));
        const asks2 = sendMessage.mock.calls.filter(
            (c) => c[0] === SIGNALING_TYPE_CODE_TERMINAL_COPILOT_ASK,
        );
        const third = (asks2[2][1] as { conversation_id: string }).conversation_id;
        expect(third).not.toBe(first);
    });
});
