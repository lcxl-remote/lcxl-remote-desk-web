import { useCallback, useEffect, useRef, useState } from 'react';
import {
    SIGNALING_TYPE_CODE_TERMINAL_COMPLETE_ASK,
    SIGNALING_TYPE_CODE_TERMINAL_COMPLETE_RESULT,
} from '../desk/constants';
import type { SignalingMessage, SignalingSubscriber } from '../desk/use-desk-signaling';
import type { ExecDecision, RiskLevel, AgentError } from './use-terminal-copilot';
import type { AiProvenance } from '@/components/ai-generated-mark';

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
    /** Machine-readable AI marking for the candidates (Art.50(2)); present when the
     *  result carries AI-generated candidates, absent on an empty / failed result. */
    provenance?: AiProvenance | null;
};

// L1 known-command corpus: common command lines offered as an instant suggestion
// when nothing in the session history matches. Kept small and shell-family aware
// (the remote re-verifies anyway — this is only a zero-latency UI hint).
const POSIX_KNOWN_COMMANDS: readonly string[] = [
    'systemctl status ', 'systemctl restart ', 'systemctl start ', 'systemctl stop ',
    'journalctl -u ', 'journalctl -xe', 'docker ps', 'docker logs ', 'docker compose up -d',
    'git status', 'git log --oneline', 'git pull', 'git push', 'git diff',
    'ls -la', 'ps aux', 'df -h', 'free -h', 'tail -f ', 'grep -rn ', 'kill -9 ',
    'cd ', 'cat ', 'chmod +x ', 'curl -s ', 'netstat -ltnp', 'ss -ltnp',
];
const WINDOWS_KNOWN_COMMANDS: readonly string[] = [
    'Get-Service ', 'Restart-Service ', 'Stop-Service ', 'Start-Service ',
    'Get-Process ', 'Stop-Process -Name ', 'Get-ChildItem ', 'Get-Content ',
    'Get-EventLog -LogName ', 'Test-NetConnection ', 'Get-NetTCPConnection',
    'ipconfig /all', 'tasklist', 'netstat -ano', 'cd ',
];

/** The L1 known-command corpus for a shell family (best-effort, OS-aware). */
export function commonCommandsFor(shell: string): readonly string[] {
    return /cmd|powershell|pwsh/i.test(shell) ? WINDOWS_KNOWN_COMMANDS : POSIX_KNOWN_COMMANDS;
}

/** First entry in `pool` that strictly extends `prefix`, returned as the suffix. */
function firstExtension(prefix: string, pool: readonly string[]): string | null {
    for (let i = pool.length - 1; i >= 0; i -= 1) {
        const cmd = pool[i];
        if (cmd.length > prefix.length && cmd.startsWith(prefix)) {
            return cmd.slice(prefix.length);
        }
    }
    return null;
}

/**
 * Pick the L1 instant (non-AI, zero-latency) ghost completion for `prefix`. The
 * most-recent matching session-history entry wins; failing that, the known-command
 * corpus is consulted. Returns the suffix (the ghost text), or null. Pure so it is
 * unit-testable and so the component can show a suggestion before the AI round-trip
 * lands.
 */
export function pickLocalGhost(
    prefix: string,
    history: readonly string[],
    known: readonly string[] = [],
): string | null {
    if (!prefix) return null;
    // Session history is the strongest local signal (most-recent first).
    return firstExtension(prefix, history) ?? firstExtension(prefix, known);
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
    /** Manager-only user-selected completion model. Omitted (null/undefined) when
     *  the completion model selector is hidden (open-source signal); the server then
     *  resolves the default, keeping the flow identical across both signaling
     *  targets. */
    modelId?: number | null;
    /** Manager-only active-organization hint. Set only in the console's org view;
     *  omitted by the personal view and the open-source control end, so no `org_id`
     *  rides the wire and the ask resolves personal-scoped exactly as before. */
    orgId?: number;
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
    modelId,
    orgId,
}: UseTerminalCompleteProps) {
    const [completions, setCompletions] = useState<CommandCompletion[]>([]);
    // The prefix the current `completions` were computed for; the component
    // renders ghost text only while its live input still equals this.
    const [completionPrefix, setCompletionPrefix] = useState<string>('');
    // AI marking for the current candidates (Art.50(2)); carried so the ghost's
    // visible mark can name the producing model. Null does not mean "not AI" —
    // an AI candidate is marked by being shown (fail-closed); this only enriches
    // the tooltip when the model / timestamp is known.
    const [provenance, setProvenance] = useState<AiProvenance | null>(null);
    const activeRequestRef = useRef<string | null>(null);
    const sentPrefixRef = useRef<string>('');
    const timerRef = useRef<number | undefined>(undefined);
    // The manager-only model / org hints, held in refs so the debounced ask reads
    // the latest values without re-creating `requestCompletion` on every change
    // (completion asks fire on keystrokes, not on selector changes).
    const modelIdRef = useRef<number | null | undefined>(modelId);
    modelIdRef.current = modelId;
    const orgIdRef = useRef<number | undefined>(orgId);
    orgIdRef.current = orgId;

    const clear = useCallback(() => {
        if (timerRef.current !== undefined) window.clearTimeout(timerRef.current);
        timerRef.current = undefined;
        activeRequestRef.current = null;
        setCompletions([]);
        setCompletionPrefix('');
        setProvenance(null);
    }, []);

    const requestCompletion = useCallback(
        (prefix: string, context: TerminalCompletionContext) => {
            if (timerRef.current !== undefined) window.clearTimeout(timerRef.current);
            // Too short / no target: drop any stale suggestion and do not ask.
            if (!connectionId || prefix.trim().length < MIN_PREFIX_LEN) {
                activeRequestRef.current = null;
                setCompletions([]);
                setCompletionPrefix('');
                setProvenance(null);
                return;
            }
            timerRef.current = window.setTimeout(() => {
                const requestId = sendMessage(
                    SIGNALING_TYPE_CODE_TERMINAL_COMPLETE_ASK,
                    {
                        prefix,
                        context,
                        // Absent (undefined) unless the manager completion selector
                        // supplied a choice; the open-source server ignores it.
                        model_id: modelIdRef.current ?? undefined,
                        // Absent (undefined) unless the console's org view supplied
                        // it; a validated hint, ignored open-source.
                        org_id: orgIdRef.current ?? undefined,
                    },
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
                setProvenance(null);
                return;
            }
            setCompletions(result.completions ?? []);
            setCompletionPrefix(sentPrefixRef.current);
            setProvenance(result.provenance ?? null);
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
        /** AI marking for the current candidates (Art.50(2)); enriches the ghost
         *  mark's model tooltip when known. */
        provenance,
        /** The first non-blocked AI candidate, or null. */
        best: firstShowable(completions),
        requestCompletion,
        clear,
    };
}
