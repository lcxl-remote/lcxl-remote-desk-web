import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import {
    useTerminalComplete,
    pickLocalGhost,
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
