import { useCallback, useEffect, useRef, useState } from 'react';
import {
    SIGNALING_TYPE_CODE_TERMINAL_COMPLETE_ASK,
    SIGNALING_TYPE_CODE_TERMINAL_COMPLETE_RESULT,
} from '../desk/constants';
import type { SignalingMessage, SignalingSubscriber } from '../desk/use-desk-signaling';
import type { ExecDecision, RiskLevel, AgentError } from './use-terminal-copilot';

// Wire types — mirror `desk_agent_protocol::terminal_complete`. They ride the
// `TerminalComplete{Ask,Result}` signaling types as `signaling_data`; like the
// copilot payloads they are not part of the REST OpenAPI surface.

/** Non-authoritative completion context — a prompt hint only. The server
 *  re-redacts and length-caps it before any model dial. */
export type TerminalCompletionContext = {
    os: string;
    shell: string;
    cwd?: string | null;
    recent_output: string;
};

/** One completion candidate. `risk` / `decision` are server-computed over the
 *  full command (`prefix` + `completion`); the control end gates its actions on
 *  `decision` (a `blocked` candidate is never shown). */
export type CommandCompletion = {
    completion: string;
    note: string;
    risk: RiskLevel;
    decision: ExecDecision;
};

export type TerminalCompleteResult = {
    request_id: string;
    completions: CommandCompletion[];
    error?: AgentError | null;
};

/**
 * Pick the L1 instant (non-AI, zero-latency) ghost completion for `prefix` from
 * recent command history: the most-recent history entry that strictly extends the
 * prefix, returned as the suffix. Pure so it is unit-testable and so the component
 * can show a suggestion before the AI round-trip lands.
 */
export function pickLocalGhost(prefix: string, history: readonly string[]): string | null {
    if (!prefix) return null;
    // Most-recent first; a history entry must extend (not equal) the prefix.
    for (let i = history.length - 1; i >= 0; i -= 1) {
        const cmd = history[i];
        if (cmd.length > prefix.length && cmd.startsWith(prefix)) {
            return cmd.slice(prefix.length);
        }
    }
    return null;
}

/** A candidate is showable as ghost text only when the server did not block it. */
function firstShowable(completions: CommandCompletion[]): CommandCompletion | null {
    return completions.find((c) => c.decision !== 'blocked') ?? null;
}

/** Shortest prefix worth asking the AI about (debounced keystroke traffic). */
const MIN_PREFIX_LEN = 2;

type UseTerminalCompleteProps = {
    /** The target host's connection id; the ask rides the outer
     *  `to_connection_id`, resolved + authorized server-side (never in payload). */
    connectionId: string | null;
    subscribe: (handler: SignalingSubscriber) => () => void;
    sendMessage: (
        type: number,
        data: unknown,
        connectionId?: string,
        requestId?: string,
    ) => string;
    /** Debounce before an L2 AI ask fires, coalescing rapid keystrokes. */
    debounceMs?: number;
};

/**
 * Drives in-terminal AI command completion over control-plane signaling. A
 * debounced `requestCompletion` sends a `TerminalCompleteAsk`; the single
 * `TerminalCompleteResult` is matched by `request_id` (stale responses, e.g. for a
 * prefix the operator has already typed past, are discarded). The result carries
 * the prefix it was computed for via `completionPrefix`, so the component shows
 * ghost text only while the current input still equals that prefix. A failed
 * result (disabled fleet / quota) simply clears the suggestion — completion is a
 * best-effort assist, never an error the operator must act on.
 */
export function useTerminalComplete({
    connectionId,
    subscribe,
    sendMessage,
    debounceMs = 180,
}: UseTerminalCompleteProps) {
    const [completions, setCompletions] = useState<CommandCompletion[]>([]);
    // The prefix the current `completions` were computed for; the component
    // renders ghost text only while its live input still equals this.
    const [completionPrefix, setCompletionPrefix] = useState<string>('');
    const activeRequestRef = useRef<string | null>(null);
    const sentPrefixRef = useRef<string>('');
    const timerRef = useRef<number | undefined>(undefined);

    const clear = useCallback(() => {
        if (timerRef.current !== undefined) window.clearTimeout(timerRef.current);
        timerRef.current = undefined;
        activeRequestRef.current = null;
        setCompletions([]);
        setCompletionPrefix('');
    }, []);

    const requestCompletion = useCallback(
        (prefix: string, context: TerminalCompletionContext) => {
            if (timerRef.current !== undefined) window.clearTimeout(timerRef.current);
            // Too short / no target: drop any stale suggestion and do not ask.
            if (!connectionId || prefix.trim().length < MIN_PREFIX_LEN) {
                activeRequestRef.current = null;
                setCompletions([]);
                setCompletionPrefix('');
                return;
            }
            timerRef.current = window.setTimeout(() => {
                const requestId = sendMessage(
                    SIGNALING_TYPE_CODE_TERMINAL_COMPLETE_ASK,
                    { prefix, context },
                    connectionId,
                );
                activeRequestRef.current = requestId;
                sentPrefixRef.current = prefix;
            }, debounceMs);
        },
        [connectionId, sendMessage, debounceMs],
    );

    useEffect(() => {
        const handle = (message: SignalingMessage) => {
            if (message.signaling_type !== SIGNALING_TYPE_CODE_TERMINAL_COMPLETE_RESULT) return;
            const result = message.signaling_data as TerminalCompleteResult | null;
            if (!result || result.request_id !== activeRequestRef.current) return;
            activeRequestRef.current = null;
            if (result.error) {
                // Disabled / rate-limited / failed: stay quiet, no suggestion.
                setCompletions([]);
                setCompletionPrefix('');
                return;
            }
            setCompletions(result.completions ?? []);
            setCompletionPrefix(sentPrefixRef.current);
        };
        return subscribe(handle);
    }, [subscribe]);

    // Drop any pending timer on unmount.
    useEffect(
        () => () => {
            if (timerRef.current !== undefined) window.clearTimeout(timerRef.current);
        },
        [],
    );

    return {
        /** Best-first AI candidates for `completionPrefix`. */
        completions,
        /** The prefix `completions` were computed for. */
        completionPrefix,
        /** The first non-blocked AI candidate, or null. */
        best: firstShowable(completions),
        requestCompletion,
        clear,
    };
}
