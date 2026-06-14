import { describe, it, expect, vi, beforeEach } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useDeskDiagnose, extractStreamingSummary } from './use-desk-diagnose';
import type { DiagnoseEvent } from './use-desk-diagnose';
import type { SignalingMessage } from './use-desk-signaling';
import {
    SIGNALING_TYPE_CODE_DIAGNOSE,
    SIGNALING_TYPE_CODE_DIAGNOSE_EVENT,
    SIGNALING_TYPE_CODE_DIAGNOSE_CANCEL,
} from './constants';

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
});

describe('useDeskDiagnose', () => {
    it('start sends a Diagnose request and enters the running phase', () => {
        const { result } = renderHook(() =>
            useDeskDiagnose({ deskId: 'desk-1', lastMessage: null, sendMessage }),
        );

        act(() => result.current.start('why slow?', { includeScreen: true }));

        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_DIAGNOSE,
            { question: 'why slow?', include_screen: true, context_kinds: [] },
            'desk-1',
        );
        expect(result.current.state.phase).toBe('running');
        expect(result.current.state.requestId).toBe('req-1');
    });

    it('aggregates status + partial frames in order, then resolves on final', () => {
        const { result, rerender } = renderHook(
            ({ msg }: { msg: SignalingMessage | null }) =>
                useDeskDiagnose({ deskId: 'desk-1', lastMessage: msg, sendMessage }),
            { initialProps: { msg: null as SignalingMessage | null } },
        );

        act(() => result.current.start('why?', {}));

        rerender({ msg: frame({ request_id: 'req-1', seq: 0, kind: 'status', status: 'collecting' }) });
        expect(result.current.state.status).toBe('collecting');

        rerender({ msg: frame({ request_id: 'req-1', seq: 1, kind: 'partial', partial_summary: 'Port ' }) });
        rerender({ msg: frame({ request_id: 'req-1', seq: 2, kind: 'partial', partial_summary: '8080 busy' }) });
        expect(result.current.state.partialSummary).toBe('Port 8080 busy');

        rerender({
            msg: frame({
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
        });
        expect(result.current.state.phase).toBe('done');
        expect(result.current.state.result?.summary).toBe('Port conflict');
        expect(result.current.state.result?.collected).toEqual(['network.ports']);
    });

    it('ignores frames for a different request and stale seq numbers', () => {
        const { result, rerender } = renderHook(
            ({ msg }: { msg: SignalingMessage | null }) =>
                useDeskDiagnose({ deskId: 'desk-1', lastMessage: msg, sendMessage }),
            { initialProps: { msg: null as SignalingMessage | null } },
        );
        act(() => result.current.start('why?', {}));

        // Wrong request id — ignored.
        rerender({ msg: frame({ request_id: 'other', seq: 0, kind: 'partial', partial_summary: 'X' }) });
        expect(result.current.state.partialSummary).toBe('');

        rerender({ msg: frame({ request_id: 'req-1', seq: 1, kind: 'partial', partial_summary: 'A' }) });
        // Stale seq (<= last applied) — ignored.
        rerender({ msg: frame({ request_id: 'req-1', seq: 1, kind: 'partial', partial_summary: 'dup' }) });
        rerender({ msg: frame({ request_id: 'req-1', seq: 0, kind: 'partial', partial_summary: 'old' }) });
        expect(result.current.state.partialSummary).toBe('A');
    });

    it('an error frame moves to the error phase with the message', () => {
        const { result, rerender } = renderHook(
            ({ msg }: { msg: SignalingMessage | null }) =>
                useDeskDiagnose({ deskId: 'desk-1', lastMessage: msg, sendMessage }),
            { initialProps: { msg: null as SignalingMessage | null } },
        );
        act(() => result.current.start('why?', {}));
        rerender({
            msg: frame({
                request_id: 'req-1',
                seq: 0,
                kind: 'error',
                error: { kind: 'redaction_failed', message: 'redaction failed', retryable: false, safe_for_model: true },
            }),
        });
        expect(result.current.state.phase).toBe('error');
        expect(result.current.state.error).toBe('redaction failed');
    });

    it('handoff sends DiagnoseCancel with the diagnosis id and keeps the result', () => {
        const { result, rerender } = renderHook(
            ({ msg }: { msg: SignalingMessage | null }) =>
                useDeskDiagnose({ deskId: 'desk-1', lastMessage: msg, sendMessage }),
            { initialProps: { msg: null as SignalingMessage | null } },
        );
        act(() => result.current.start('why?', {}));
        rerender({ msg: frame({ request_id: 'req-1', seq: 0, kind: 'partial', partial_summary: 'partial text' }) });

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
        rerender({ msg: frame({ request_id: 'req-1', seq: 1, kind: 'partial', partial_summary: ' more' }) });
        expect(result.current.state.partialSummary).toBe('partial text');
    });

    it('reset returns to the idle question form', () => {
        const { result } = renderHook(() =>
            useDeskDiagnose({ deskId: 'desk-1', lastMessage: null, sendMessage }),
        );
        act(() => result.current.start('why?', {}));
        act(() => result.current.reset());
        expect(result.current.state.phase).toBe('idle');
        expect(result.current.state.requestId).toBeNull();
    });
});
