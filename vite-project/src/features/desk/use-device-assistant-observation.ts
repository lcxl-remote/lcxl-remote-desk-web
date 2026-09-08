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
    phase: 'idle' | 'scheduled' | 'pending' | 'ready' | 'error';
    requestId: string | null;
    outcome: AgentOutcome | null;
};

export type ObservationRoot = {
    token: string; snapshot_id: string; expires_at: string;
    object_kind: 'desktop_session' | 'application' | 'window';
};
export type SelectableApplication = { objectRef: ObservationRoot; name: string };

export function selectableApplications(entry: ObservationEntry): SelectableApplication[] {
    if (entry.outcome?.status !== 'ok') return [];
    const data = entry.outcome.data as { ReadContext?: { DesktopUiInspect?: { nodes?: unknown[] } } } | null;
    const nodes = data?.ReadContext?.DesktopUiInspect?.nodes;
    if (!Array.isArray(nodes)) return [];
    return nodes.flatMap((node) => {
        if (!node || typeof node !== 'object') return [];
        const value = node as { object_ref?: ObservationRoot; role?: string; name?: unknown };
        const ref = value.object_ref;
        if (value.role !== 'application' || ref?.object_kind !== 'application'
            || typeof ref.token !== 'string' || !ref.token
            || typeof ref.snapshot_id !== 'string' || !ref.snapshot_id
            || typeof ref.expires_at !== 'string' || !Number.isFinite(Date.parse(ref.expires_at))) return [];
        return [{ objectRef: ref, name: typeof value.name === 'string' ? value.name : '' }];
    });
}

export type OwnerSelectableWindow = {
    objectRef: {
        token: string;
        snapshot_id: string;
        object_kind: 'window';
        expires_at: string;
    };
    title: string | null;
};

export function ownerSelectableWindows(entry: ObservationEntry): OwnerSelectableWindow[] {
    if (entry.outcome?.status !== 'ok') return [];
    const output = entry.outcome.data as {
        ReadContext?: { DesktopUiInspect?: { owner_selectable_windows?: unknown[] } };
    } | null;
    const candidates = output?.ReadContext?.DesktopUiInspect?.owner_selectable_windows;
    if (!Array.isArray(candidates)) return [];
    return candidates.flatMap((candidate) => {
        const value = candidate as {
            object_ref?: OwnerSelectableWindow['objectRef'];
            title?: unknown;
        };
        const objectRef = value.object_ref;
        if (
            !objectRef
            || objectRef.object_kind !== 'window'
            || !objectRef.token
            || !objectRef.snapshot_id
            || !objectRef.expires_at
        ) return [];
        return [{
            objectRef,
            title: typeof value.title === 'string' && value.title.trim() ? value.title : null,
        }];
    });
}

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
    enabled?: boolean;
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
    enabled = true,
}: Props) {
    const [entries, setEntries] = useState<Record<ObservationKind, ObservationEntry>>({
        desktop_session_inspect: idleEntry(),
        desktop_ui_inspect: idleEntry(),
    });
    const [applications, setApplications] = useState<SelectableApplication[]>([]);
    const applicationCatalogRequested = useRef(false);
    const pending = useRef(new Map<string, ObservationKind>());
    const timers = useRef(new Map<string, number>());
    const delayedTimer = useRef<number | null>(null);
    const [remainingSeconds, setRemainingSeconds] = useState(0);
    const scope = useRef({ deskId, enabled });
    scope.current = { deskId, enabled };

    const cancelDelayedUi = useCallback(() => {
        if (delayedTimer.current !== null) window.clearTimeout(delayedTimer.current);
        delayedTimer.current = null;
        setRemainingSeconds(0);
        setEntries((current) => current.desktop_ui_inspect.phase === 'scheduled'
            ? { ...current, desktop_ui_inspect: idleEntry() } : current);
    }, []);

    useEffect(() => subscribe((message: SignalingMessage) => {
        if (!scope.current.enabled || scope.current.deskId !== deskId) return;
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
    }), [subscribe, deskId]);

    useEffect(() => {
        cancelDelayedUi();
        applicationCatalogRequested.current = false;
        setApplications([]);
        setEntries({ desktop_session_inspect: idleEntry(), desktop_ui_inspect: idleEntry() });
        return () => {
            if (delayedTimer.current !== null) window.clearTimeout(delayedTimer.current);
            delayedTimer.current = null;
            timers.current.forEach((timer) => window.clearTimeout(timer));
            timers.current.clear();
            pending.current.clear();
        };
    }, [deskId, enabled, cancelDelayedUi]);

    const invoke = useCallback((kind: ObservationKind, params: Record<string, unknown>) => {
        if (!deskId || !enabled || scope.current.deskId !== deskId || !scope.current.enabled) return null;
        if ([...pending.current.values()].includes(kind)) return null;
        if (kind === 'desktop_ui_inspect' && delayedTimer.current !== null) return null;
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
                reason: 'AI Assistant read-only observation preview',
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
    }, [deskId, enabled, sendMessage, timeoutMs]);

    const inspectSession = useCallback(() => invoke('desktop_session_inspect', {
        include_active_application: true,
    }), [invoke]);

    const inspectUi = useCallback((root: ObservationRoot | null = null) => {
        if (root && (!Number.isFinite(Date.parse(root.expires_at)) || Date.parse(root.expires_at) <= Date.now())) return null;
        return invoke('desktop_ui_inspect', {
        root,
        max_depth: 6,
        max_nodes: 300,
        max_bytes: 262_144,
    }); }, [invoke]);

    const listApplications = useCallback(() => {
        if (delayedTimer.current !== null || pending.current.size > 0) return;
        applicationCatalogRequested.current = true;
        setApplications([]);
        if (!inspectSession()) applicationCatalogRequested.current = false;
    }, [inspectSession]);

    const sessionOutput = entries.desktop_session_inspect.outcome?.status === 'ok'
        ? (entries.desktop_session_inspect.outcome.data as { ReadContext?: { DesktopSessionInspect?: { os?: string; session?: ObservationRoot } } })?.ReadContext?.DesktopSessionInspect
        : undefined;
    useEffect(() => {
        if (!applicationCatalogRequested.current) return;
        if (entries.desktop_session_inspect.phase === 'error') {
            applicationCatalogRequested.current = false;
        } else if (entries.desktop_session_inspect.phase === 'ready') {
            applicationCatalogRequested.current = false;
            const root = sessionOutput?.session;
            if (sessionOutput?.os === 'macos' && root?.object_kind === 'desktop_session'
                && root.token && root.snapshot_id) inspectUi(root);
        }
    }, [entries.desktop_session_inspect, sessionOutput, inspectUi]);
    useEffect(() => {
        const candidates = selectableApplications(entries.desktop_ui_inspect);
        if (candidates.length) setApplications(candidates);
    }, [entries.desktop_ui_inspect]);

    const scheduleUi = useCallback(() => {
        if (!deskId || !enabled || delayedTimer.current !== null
            || [...pending.current.values()].includes('desktop_ui_inspect')) return;
        const targetDeskId = deskId;
        const due = Date.now() + 5_000;
        setEntries((current) => ({ ...current, desktop_ui_inspect: { ...idleEntry(), phase: 'scheduled' } }));
        setRemainingSeconds(5);
        const tick = () => {
            if (scope.current.deskId !== targetDeskId || !scope.current.enabled) {
                cancelDelayedUi();
                return;
            }
            const remaining = Math.max(0, Math.ceil((due - Date.now()) / 1_000));
            setRemainingSeconds(remaining);
            if (remaining === 0) {
                delayedTimer.current = null;
                inspectUi();
            } else {
                delayedTimer.current = window.setTimeout(tick, 1_000);
            }
        };
        delayedTimer.current = window.setTimeout(tick, 1_000);
    }, [cancelDelayedUi, deskId, enabled, inspectUi]);

    return { entries, inspectSession, inspectUi, scheduleUi, cancelDelayedUi, remainingSeconds,
        applications, listApplications, applicationSelectionAvailable: !sessionOutput?.os || sessionOutput.os === 'macos' };
}
