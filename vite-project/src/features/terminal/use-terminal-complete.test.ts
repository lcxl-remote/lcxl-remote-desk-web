import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import {
    useTerminalComplete,
    pickLocalGhost,
    commonCommandsFor,
    type TerminalCompleteResult,
    type TerminalCompletionContext,
} from './use-terminal-complete';
import type { SignalingMessage } from '../desk/use-desk-signaling';
import {
    SIGNALING_TYPE_CODE_TERMINAL_COMPLETE_ASK,
    SIGNALING_TYPE_CODE_TERMINAL_COMPLETE_RESULT,
} from '../desk/constants';

// `sendMessage` returns the wire request_id the hook keys its result on.
const sendMessage = vi.fn(() => 'req-1');

beforeEach(() => {
    sendMessage.mockClear();
    vi.useFakeTimers();
});
afterEach(() => {
    vi.useRealTimers();
});

const ctx: TerminalCompletionContext = {
    os: 'linux',
    shell: 'bash',
    recent_output: '$ systemctl',
};

function resultFrame(result: TerminalCompleteResult): SignalingMessage {
    return {
        request_id: result.request_id,
        signaling_type: SIGNALING_TYPE_CODE_TERMINAL_COMPLETE_RESULT,
        signaling_data: result,
    };
}

function renderComplete(connectionId: string | null = 'conn-1') {
    const handlers = new Set<(m: SignalingMessage) => void>();
    const subscribe = (h: (m: SignalingMessage) => void) => {
        handlers.add(h);
        return () => {
            handlers.delete(h);
        };
    };
    const view = renderHook(() =>
        useTerminalComplete({ connectionId, subscribe, sendMessage, debounceMs: 100 }),
    );
    const feed = (msg: SignalingMessage) =>
        act(() => {
            handlers.forEach((h) => h(msg));
        });
    return { ...view, feed };
}

describe('pickLocalGhost', () => {
    it('returns the suffix of the most-recent extending history entry', () => {
        const hist = ['systemctl status nginx', 'ls -la', 'systemctl restart nginx'];
        expect(pickLocalGhost('systemctl ', hist)).toBe('restart nginx');
    });
    it('ignores an exact-match (no suffix) and non-extending entries', () => {
        expect(pickLocalGhost('ls -la', ['ls -la'])).toBeNull();
        expect(pickLocalGhost('cat', ['ls -la'])).toBeNull();
        expect(pickLocalGhost('', ['ls -la'])).toBeNull();
    });
    it('falls back to the known-command corpus when history has no match', () => {
        const known = commonCommandsFor('bash');
        // No history match, but the corpus extends "systemctl ".
        expect(pickLocalGhost('systemctl resta', [], known)).toBe('rt ');
    });
    it('prefers a history match over the known-command corpus', () => {
        const known = commonCommandsFor('bash');
        expect(pickLocalGhost('systemctl ', ['systemctl daemon-reload'], known)).toBe(
            'daemon-reload',
        );
    });
    it('commonCommandsFor is shell-family aware', () => {
        expect(commonCommandsFor('pwsh').some((c) => c.startsWith('Get-Service'))).toBe(true);
        expect(commonCommandsFor('bash').some((c) => c.startsWith('systemctl'))).toBe(true);
    });
});

describe('useTerminalComplete', () => {
    it('debounces and sends one TerminalCompleteAsk to the target', () => {
        const { result } = renderComplete();
        act(() => result.current.requestCompletion('systemctl ', ctx));
        // Nothing sent until the debounce elapses.
        expect(sendMessage).not.toHaveBeenCalled();
        act(() => vi.advanceTimersByTime(100));
        expect(sendMessage).toHaveBeenCalledTimes(1);
        const [type, data, connectionId] = sendMessage.mock.calls[0];
        expect(type).toBe(SIGNALING_TYPE_CODE_TERMINAL_COMPLETE_ASK);
        expect(connectionId).toBe('conn-1');
        expect((data as { prefix: string }).prefix).toBe('systemctl ');
    });

    it('omits model_id / org_id when not supplied (open-source / personal parity)', () => {
        const { result } = renderComplete();
        act(() => result.current.requestCompletion('systemctl ', ctx));
        act(() => vi.advanceTimersByTime(100));
        const data = sendMessage.mock.calls[0][1] as {
            model_id?: number;
            org_id?: number;
        };
        expect(data.model_id).toBeUndefined();
        expect(data.org_id).toBeUndefined();
    });

    it('carries the manager model_id and org_id hints when supplied', () => {
        const subscribe = () => () => {};
        const { result } = renderHook(() =>
            useTerminalComplete({
                connectionId: 'conn-1',
                subscribe,
                sendMessage,
                debounceMs: 100,
                modelId: 7,
                orgId: 4,
            }),
        );
        act(() => result.current.requestCompletion('systemctl ', ctx));
        act(() => vi.advanceTimersByTime(100));
        const data = sendMessage.mock.calls[0][1] as {
            model_id?: number;
            org_id?: number;
        };
        expect(data.model_id).toBe(7);
        expect(data.org_id).toBe(4);
    });

    it('coalesces rapid keystrokes into a single ask for the latest prefix', () => {
        const { result } = renderComplete();
        act(() => result.current.requestCompletion('sy', ctx));
        act(() => result.current.requestCompletion('systemctl ', ctx));
        act(() => vi.advanceTimersByTime(100));
        expect(sendMessage).toHaveBeenCalledTimes(1);
        expect((sendMessage.mock.calls[0][1] as { prefix: string }).prefix).toBe('systemctl ');
    });

    it('applies a matching result and exposes the first non-blocked candidate', () => {
        const { result, feed } = renderComplete();
        act(() => result.current.requestCompletion('systemctl ', ctx));
        act(() => vi.advanceTimersByTime(100));
        feed(
            resultFrame({
                request_id: 'req-1',
                completions: [
                    { completion: 'status nginx', note: 'status', risk: 'low', decision: 'not_executable' },
                ],
            }),
        );
        expect(result.current.completionPrefix).toBe('systemctl ');
        expect(result.current.best?.completion).toBe('status nginx');
    });

    it('carries the AI marking (Art.50(2)) so the ghost can name the model', () => {
        const { result, feed } = renderComplete();
        act(() => result.current.requestCompletion('systemctl ', ctx));
        act(() => vi.advanceTimersByTime(100));
        feed(
            resultFrame({
                request_id: 'req-1',
                completions: [
                    { completion: 'status nginx', note: 'status', risk: 'low', decision: 'not_executable' },
                ],
                provenance: { model_id: 'gpt-4o', marking_scheme: 'lcxl-ai-provenance/1' },
            }),
        );
        expect(result.current.provenance?.model_id).toBe('gpt-4o');
    });

    it('leaves provenance null when the result omits it (fail-closed: the ghost still marks by source)', () => {
        const { result, feed } = renderComplete();
        act(() => result.current.requestCompletion('systemctl ', ctx));
        act(() => vi.advanceTimersByTime(100));
        feed(
            resultFrame({
                request_id: 'req-1',
                completions: [
                    { completion: 'status nginx', note: 'status', risk: 'low', decision: 'not_executable' },
                ],
            }),
        );
        expect(result.current.provenance).toBeNull();
    });

    it('discards a stale result whose request_id is not the active one', () => {
        const { result, feed } = renderComplete();
        act(() => result.current.requestCompletion('systemctl ', ctx));
        act(() => vi.advanceTimersByTime(100));
        feed(
            resultFrame({
                request_id: 'stale-req',
                completions: [
                    { completion: 'x', note: '', risk: 'low', decision: 'not_executable' },
                ],
            }),
        );
        expect(result.current.best).toBeNull();
    });

    it('stays quiet on a failed result (disabled / rate-limited)', () => {
        const { result, feed } = renderComplete();
        act(() => result.current.requestCompletion('systemctl ', ctx));
        act(() => vi.advanceTimersByTime(100));
        feed(
            resultFrame({
                request_id: 'req-1',
                completions: [],
                error: { kind: 'UnsupportedCapability', message: 'disabled', retryable: false, safe_for_model: true },
            }),
        );
        expect(result.current.best).toBeNull();
        expect(result.current.completionPrefix).toBe('');
    });

    it('does not ask for a too-short prefix and clears any suggestion', () => {
        const { result } = renderComplete();
        act(() => result.current.requestCompletion('s', ctx));
        act(() => vi.advanceTimersByTime(100));
        expect(sendMessage).not.toHaveBeenCalled();
    });
});
