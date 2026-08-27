import { useCallback, useEffect, useRef, useState } from 'react';

import {
    SIGNALING_TYPE_CODE_AGENT_CAPABILITY_COMPLETED,
    SIGNALING_TYPE_CODE_INVOKE_AGENT_CAPABILITY,
} from './constants';
import type { SignalingMessage, SignalingSubscriber } from './use-desk-signaling';

export type ObservationKind = 'desktop_session_inspect' | 'desktop_ui_inspect';

export type AgentError = {
    kind: string;
    message: string;
    retryable: boolean;
    safe_for_model: boolean;
    error_code?: number | null;
};

export type AgentOutcome =
    | { status: 'ok'; data: unknown }
    | { status: 'err'; data: AgentError };

type WireAgentOutcome = AgentOutcome
    | { status: 'Ok'; data: unknown }
    | { status: 'Err'; data: AgentError }
    | { Ok: unknown }
    | { Err: AgentError };

export function normalizeAgentOutcome(raw: WireAgentOutcome): AgentOutcome {
    if ('status' in raw) {
        return raw.status.toLowerCase() === 'ok'
            ? { status: 'ok', data: raw.data }
            : { status: 'err', data: raw.data as AgentError };
    }
    if ('Ok' in raw) return { status: 'ok', data: raw.Ok };
    return { status: 'err', data: raw.Err };
}

export type ObservationEntry = {
    phase: 'idle' | 'pending' | 'ready' | 'error';
    requestId: string | null;
    outcome: AgentOutcome | null;
};

type SendMessage = (
    type: number,
    data: unknown,
    toConnectionId?: string,
    requestId?: string,
) => string;

type Props = {
    deskId: string | null;
    subscribe: (handler: SignalingSubscriber) => () => void;
    sendMessage: SendMessage;
    timeoutMs?: number;
};

const idleEntry = (): ObservationEntry => ({
    phase: 'idle',
    requestId: null,
    outcome: null,
});

export function useDeviceAssistantObservation({
    deskId,
    subscribe,
    sendMessage,
    timeoutMs = 15_000,
}: Props) {
    const [entries, setEntries] = useState<Record<ObservationKind, ObservationEntry>>({
        desktop_session_inspect: idleEntry(),
        desktop_ui_inspect: idleEntry(),
    });
    const pending = useRef(new Map<string, ObservationKind>());
    const timers = useRef(new Map<string, number>());

    useEffect(() => subscribe((message: SignalingMessage) => {
        if (message.signaling_type !== SIGNALING_TYPE_CODE_AGENT_CAPABILITY_COMPLETED) return;
        const requestId = message.request_id;
        if (!requestId) return;
        const kind = pending.current.get(requestId);
        if (!kind) return;

        pending.current.delete(requestId);
        const timer = timers.current.get(requestId);
        if (timer !== undefined) window.clearTimeout(timer);
        timers.current.delete(requestId);

        const outcome = normalizeAgentOutcome(message.signaling_data as WireAgentOutcome);
        setEntries((current) => ({
            ...current,
            [kind]: {
                phase: outcome?.status === 'ok' ? 'ready' : 'error',
                requestId,
                outcome,
            },
        }));
    }), [subscribe]);

    useEffect(() => () => {
        timers.current.forEach((timer) => window.clearTimeout(timer));
        timers.current.clear();
        pending.current.clear();
    }, []);

    const invoke = useCallback((kind: ObservationKind, params: Record<string, unknown>) => {
        if (!deskId) return null;
        const requestId = sendMessage(
            SIGNALING_TYPE_CODE_INVOKE_AGENT_CAPABILITY,
            {
                operation: {
                    risk_hint: null,
                    input: {
                        kind: 'read_context',
                        params: { kind: { kind, params } },
                    },
                },
                reason: 'Device Assistant read-only observation preview',
            },
            deskId,
        );
        pending.current.set(requestId, kind);
        setEntries((current) => ({
            ...current,
            [kind]: { phase: 'pending', requestId, outcome: null },
        }));
        const timer = window.setTimeout(() => {
            if (!pending.current.delete(requestId)) return;
            timers.current.delete(requestId);
            setEntries((current) => ({
                ...current,
                [kind]: {
                    phase: 'error',
                    requestId,
                    outcome: {
                        status: 'err',
                        data: {
                            kind: 'timeout',
                            message: 'The host did not answer the observation request in time.',
                            retryable: true,
                            safe_for_model: true,
                        },
                    },
                },
            }));
        }, timeoutMs);
        timers.current.set(requestId, timer);
        return requestId;
    }, [deskId, sendMessage, timeoutMs]);

    const inspectSession = useCallback(() => invoke('desktop_session_inspect', {
        include_active_application: true,
    }), [invoke]);

    const inspectUi = useCallback(() => invoke('desktop_ui_inspect', {
        root: null,
        max_depth: 6,
        max_nodes: 300,
        max_bytes: 262_144,
    }), [invoke]);

    return { entries, inspectSession, inspectUi };
}
