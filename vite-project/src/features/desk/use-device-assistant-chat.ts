import type { AssistantContextUsage } from './assistant-context-meter';
import { useCallback, useEffect, useRef, useState } from 'react';
import { v4 } from 'uuid';

import type { AiProvenance } from '@/components/ai-generated-mark';
import type {
    BackgroundTaskDto,
    CapabilityGrantDto,
    ContextNoticeDto,
    PermissionDecisionBody,
    PermissionRequestDto,
} from '@/services/types';
import { deskErrorCodeEnum } from '@/services/types';
import type { DeviceAssistantEvent, DeviceAssistantVisualEvidence } from './device-assistant-event';
import {
    SIGNALING_TYPE_CODE_ASK_DEVICE_ASSISTANT,
    SIGNALING_TYPE_CODE_CANCEL_DEVICE_ASSISTANT,
    SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_CONTEXT_UPDATED,
    SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_OBJECT_CONTEXT_UPDATED,
    SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_SESSION_SELECTED,
    SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_UPDATED,
    SIGNALING_TYPE_CODE_SELECT_DEVICE_ASSISTANT_SESSION,
    SIGNALING_TYPE_CODE_UPDATE_DEVICE_ASSISTANT_CONTEXT,
    SIGNALING_TYPE_CODE_UPDATE_DEVICE_ASSISTANT_OBJECT_CONTEXT,
} from './constants';
import type { SignalingMessage, SignalingSubscriber } from './use-desk-signaling';
import {
    parseSessionTargetList,
    type SessionTargetDescriptor,
} from './session-target-selection';

const PREVIEW_TOOL = 'preview_computer_action';

export type DeviceAssistantMessage = {
    contextBoundaryIds?: string[];
    id: string;
    role: 'user' | 'assistant' | 'tool_result';
    text: string;
    provenance?: AiProvenance | null;
};

export type DeviceAssistantToolActivity = {
    callId: string;
    name: string;
    status: 'running' | 'ok' | 'failed';
    argumentsJson: string;
    output: string | null;
};

export type DeviceAssistantContextAttachment = {
    id: string;
    kind: string;
    providerId: string;
    capabilityId: string;
    displaySummary: string;
    createdAtUnixMs: number;
    expiresAtUnixMs: number;
    state: 'active' | 'stale';
    staleReason?: string;
};

export type DeviceAssistantWindowRef = {
    token: string;
    snapshot_id: string;
    object_kind: 'window';
    expires_at: string;
};

export type ComputerActionDraftPreview = {
    schema_version: number;
    adapter: { kind: string; version: string };
    risk: string;
    reversible: boolean;
    data_egress: boolean;
    actions: Array<{
        target: Record<string, unknown>;
        action: Record<string, unknown>;
        before_summary: string;
        after_intent: string;
        verification: string;
    }>;
};

type Props = {
    deskId: string;
    connected?: boolean;
    /// Stable device identity for browser-side conversation intent. The OSS
    /// connection id changes after a server restart, while client_id does not.
    conversationStorageScope?: string;
    subscribe: (handler: SignalingSubscriber) => () => void;
    sendMessage: (
        type: number,
        data: unknown,
        connectionId?: string,
        requestId?: string,
    ) => string;
};

function storageKey(scope: string) {
    return `device-assistant-conversation:${scope}`;
}

function parseDraft(raw: string | null | undefined): ComputerActionDraftPreview | null {
    if (!raw) return null;
    try {
        const value = JSON.parse(raw) as ComputerActionDraftPreview;
        if (value.schema_version !== 1 || !Array.isArray(value.actions)) return null;
        return value;
    } catch {
        return null;
    }
}

function upsertTool(
    tools: DeviceAssistantToolActivity[],
    next: DeviceAssistantToolActivity,
) {
    const index = tools.findIndex((tool) => tool.callId === next.callId);
    if (index === -1) return [...tools, next];
    return tools.map((tool, current) => current === index ? next : tool);
}

function upsertVisualEvidence(
    current: DeviceAssistantVisualEvidence[],
    next: DeviceAssistantVisualEvidence,
) {
    const existing = current.find((item) => item.evidence_id === next.evidence_id);
    const merged = existing?.preview_data_url && !next.preview_data_url
        ? { ...next, status: existing.status, preview_data_url: existing.preview_data_url }
        : next;
    const without = current.filter((item) => item.evidence_id !== next.evidence_id);
    return [...without, merged].slice(-32);
}

type PersistedToolCall = {
    id: string;
    name: string;
    argumentsJson: string;
};

export type DeviceAssistantTaskStatusProjection = {
    schemaVersion: number;
    revision: number;
    updatedAt: string;
    items: Array<{
        itemId: string;
        description: string;
        status: 'todo' | 'in_progress' | 'blocked' | 'done' | 'skipped';
        note?: string | null;
        lastUpdatedStepId: string;
    }>;
};

export type DeviceAssistantUnknownOutcome = {
    workId: number;
    actionRequestId: string;
    executionId: string;
    workKind: string;
};

type PersistedSnapshotMessage = {
    id: string;
    role: string;
    text: string;
    toolCallId?: string | null;
    backgroundTaskId?: string | null;
    toolCalls?: PersistedToolCall[];
};

type PersistedSnapshot = {
    terminalError?: { message: string } | null;
    contextNotices?: ContextNoticeDto[];
    contextUsage?: AssistantContextUsage | null;
    sessionId: string;
    seq: number;
    active: boolean;
    latestInputSeq?: number;
    inputRevision?: number;
    handledInputSeq?: number;
    taskStatusProjection?: DeviceAssistantTaskStatusProjection | null;
    permissionRequests?: PermissionRequestDto[];
    backgroundTasks?: BackgroundTaskDto[];
    capabilityGrants?: CapabilityGrantDto[];
    unresolvedOutcome?: DeviceAssistantUnknownOutcome | null;
    messages: PersistedSnapshotMessage[];
    messagePage?: {
        hasMore: boolean;
        nextBeforeMessageId?: string | null;
        limit: number;
    };
    contextAttachments?: DeviceAssistantContextAttachment[];
    visualEvidence?: DeviceAssistantVisualEvidence[];
};

function projectPersistedSnapshot(snapshot: PersistedSnapshot) {
    const messages: DeviceAssistantMessage[] = [];
    let tools: DeviceAssistantToolActivity[] = [];
    let draft: ComputerActionDraftPreview | null = null;
    for (const message of snapshot.messages) {
        if ((message.role === 'user' || message.role === 'assistant') && message.text) {
            messages.push({
                id: message.id,
                role: message.role,
                text: message.text,
            });
        }
        for (const call of message.toolCalls ?? []) {
            tools = upsertTool(tools, {
                callId: call.id,
                name: call.name,
                status: 'running',
                argumentsJson: call.argumentsJson,
                output: null,
            });
            if (call.name === PREVIEW_TOOL) {
                draft = parseDraft(call.argumentsJson) ?? draft;
            }
        }
        if ((message.role === 'tool' || message.role === 'untrusted_output') && message.toolCallId) {
            const existing = tools.find((tool) => tool.callId === message.toolCallId);
            const backgroundRunning = /"status"\s*:\s*"background_running"/.test(message.text);
            tools = upsertTool(tools, {
                callId: message.toolCallId,
                name: existing?.name ?? 'unknown',
                status: backgroundRunning ? 'running'
                    : /^(tool error:|not executed:|execution failed:|execution did not complete:)/i.test(message.text) ? 'failed' : 'ok',
                argumentsJson: existing?.argumentsJson ?? '{}',
                output: message.text || null,
            });
            if (!backgroundRunning && message.text && (existing?.name === 'execute_confirmed_command' || message.backgroundTaskId)) {
                messages.push({ id: message.id, role: 'tool_result', text: message.text });
            }
        }
    }
    let lastVisible: DeviceAssistantMessage | undefined;
    for (const raw of snapshot.messages) {
        lastVisible = messages.find(message => message.id === raw.id) ?? lastVisible;
        if (lastVisible && lastVisible.id !== raw.id) {
            lastVisible.contextBoundaryIds = [...(lastVisible.contextBoundaryIds ?? []), raw.id];
        }
    }
    return {
        messages,
        tools,
        draft,
        attachments: Array.isArray(snapshot.contextAttachments)
            ? snapshot.contextAttachments
            : [],
        taskStatusProjection: snapshot.taskStatusProjection ?? null,
        permissionRequests: Array.isArray(snapshot.permissionRequests)
            ? snapshot.permissionRequests
            : [],
        backgroundTasks: Array.isArray(snapshot.backgroundTasks)
            ? snapshot.backgroundTasks
            : [],
        capabilityGrants: Array.isArray(snapshot.capabilityGrants)
            ? snapshot.capabilityGrants
            : [],
        unresolvedOutcome: snapshot.unresolvedOutcome ?? null,
        pendingInputCount: Math.max(
            0,
            (snapshot.latestInputSeq ?? 0) - (snapshot.handledInputSeq ?? 0),
        ),
    };
}

export function useDeviceAssistantChat({
    deskId,
    connected,
    conversationStorageScope = deskId,
    subscribe,
    sendMessage,
}: Props) {
    const targetSelectionEnabled = connected !== undefined;
    const [contextUsage, setContextUsage] = useState<AssistantContextUsage | null>(null);
    const [contextNotices, setContextNotices] = useState<ContextNoticeDto[]>([]);
    const [messages, setMessages] = useState<DeviceAssistantMessage[]>([]);
    const [tools, setTools] = useState<DeviceAssistantToolActivity[]>([]);
    const [draft, setDraft] = useState<ComputerActionDraftPreview | null>(null);
    const [partial, setPartial] = useState('');
    const [status, setStatus] = useState('idle');
    const [error, setError] = useState<string | null>(null);
    const [attachments, setAttachments] = useState<DeviceAssistantContextAttachment[]>([]);
    const [hydrating, setHydrating] = useState(false);
    const [remoteActive, setRemoteActive] = useState(false);
    const [contextUpdating, setContextUpdating] = useState(false);
    const [taskStatusProjection, setTaskStatusProjection] =
        useState<DeviceAssistantTaskStatusProjection | null>(null);
    const [permissionRequests, setPermissionRequests] = useState<PermissionRequestDto[]>([]);
    const [backgroundTasks, setBackgroundTasks] = useState<BackgroundTaskDto[]>([]);
    const [capabilityGrants, setCapabilityGrants] = useState<CapabilityGrantDto[]>([]);
    const [unresolvedOutcome, setUnresolvedOutcome] =
        useState<DeviceAssistantUnknownOutcome | null>(null);
    const [outcomeDisposing, setOutcomeDisposing] = useState(false);
    const [permissionUpdating, setPermissionUpdating] = useState(false);
    const [grantRevoking, setGrantRevoking] = useState<string | null>(null);
    const [pendingInputCount, setPendingInputCount] = useState(0);
    const [messagePage, setMessagePage] = useState<{
        hasMore: boolean;
        nextBeforeMessageId: string | null;
    }>({ hasMore: false, nextBeforeMessageId: null });
    const [loadingOlderMessages, setLoadingOlderMessages] = useState(false);
    const [visualEvidence, setVisualEvidence] = useState<DeviceAssistantVisualEvidence[]>([]);
    const [sessionTarget, setSessionTarget] = useState<SessionTargetDescriptor | null>(null);
    const [sessionTargets, setSessionTargets] = useState<SessionTargetDescriptor[]>([]);
    const [sessionTargetReady, setSessionTargetReady] = useState(!targetSelectionEnabled);
    const [sessionTargetResolving, setSessionTargetResolving] = useState(false);
    const activeRequest = useRef<string | null>(null);
    const contextRequest = useRef<string | null>(null);
    const contextTimer = useRef<number | null>(null);
    const conversationId = useRef<string | null>(null);
    const snapshotEpoch = useRef(0);
    const snapshotRequestOrder = useRef(0);
    const snapshotWatermark = useRef<{
        conversationId: string;
        sessionId: string;
        seq: number;
        requestOrder: number;
    } | null>(null);
    const lastSeq = useRef(-1);
    const previewArgs = useRef(new Map<string, string>());
    const sessionTargetRequest = useRef<string | null>(null);

    const selectSessionTarget = useCallback((targetId?: string) => {
        if (!targetSelectionEnabled || !connected || sessionTargetRequest.current) return false;
        setSessionTargetResolving(true);
        setError(null);
        sessionTargetRequest.current = sendMessage(
            SIGNALING_TYPE_CODE_SELECT_DEVICE_ASSISTANT_SESSION,
            targetId ? { session_target_id: targetId } : {},
            deskId,
        );
        return true;
    }, [connected, deskId, sendMessage, targetSelectionEnabled]);

    useEffect(() => {
        if (!targetSelectionEnabled) return;
        if (!connected) {
            sessionTargetRequest.current = null;
            setSessionTarget(null);
            setSessionTargets([]);
            setSessionTargetReady(false);
            setSessionTargetResolving(false);
            return;
        }
        selectSessionTarget();
    }, [connected, selectSessionTarget, targetSelectionEnabled]);

    const loadSnapshot = useCallback(async (
        expectedConversationId: string,
        showHydrating = false,
        reportFailure = false,
    ) => {
        const expectedEpoch = snapshotEpoch.current;
        const expectedRequestOrder = ++snapshotRequestOrder.current;
        if (showHydrating) setHydrating(true);
        try {
            const response = await fetch(
            `/api/my/device-assistant-session?connection=${encodeURIComponent(deskId)}` +
                `&conversation=${encodeURIComponent(expectedConversationId)}`,
            { credentials: 'include', headers: { Accept: 'application/json' } },
            );
            const body = response.ok ? await response.json() : null;
            if (reportFailure && !Array.isArray(body?.data?.messages)) throw new Error('Snapshot unavailable');
            if (
                snapshotEpoch.current !== expectedEpoch
                || conversationId.current !== expectedConversationId
                || !Array.isArray(body?.data?.messages)
            ) return;
            const snapshot = body.data as PersistedSnapshot;
            if (
                typeof snapshot.sessionId !== 'string'
                || snapshot.sessionId.length === 0
                || !Number.isSafeInteger(snapshot.seq)
                || snapshot.seq < 0
            ) return;
            const watermark = snapshotWatermark.current;
            if (
                watermark
                && (
                    watermark.conversationId !== expectedConversationId
                    || (
                        watermark.sessionId === snapshot.sessionId
                        && snapshot.seq < watermark.seq
                    )
                    || (
                        watermark.sessionId !== snapshot.sessionId
                        && expectedRequestOrder <= watermark.requestOrder
                    )
                )
            ) return;
            snapshotWatermark.current = {
                conversationId: expectedConversationId,
                sessionId: snapshot.sessionId,
                seq: snapshot.seq,
                requestOrder: expectedRequestOrder,
            };
            setContextUsage(snapshot.contextUsage ?? null);
            setContextNotices([...new Map((snapshot.contextNotices ?? []).map(notice => [notice.id, notice])).values()]);
            const projected = projectPersistedSnapshot(snapshot);
            setAttachments(projected.attachments);
            setTaskStatusProjection(projected.taskStatusProjection);
            setPermissionRequests(projected.permissionRequests);
            setBackgroundTasks(projected.backgroundTasks);
            setCapabilityGrants(projected.capabilityGrants);
            setUnresolvedOutcome(projected.unresolvedOutcome);
            setPendingInputCount(projected.pendingInputCount);
            setMessagePage({
                hasMore: Boolean(snapshot.messagePage?.hasMore),
                nextBeforeMessageId: snapshot.messagePage?.nextBeforeMessageId ?? null,
            });
            setVisualEvidence((current) => (snapshot.visualEvidence ?? []).reduce(
                (items, next) => upsertVisualEvidence(items, next),
                [] as DeviceAssistantVisualEvidence[],
            ).map((next) => {
                const live = current.find((item) => item.evidence_id === next.evidence_id);
                return live?.preview_data_url && !next.preview_data_url
                    ? { ...next, status: live.status, preview_data_url: live.preview_data_url }
                    : next;
            }));
            setRemoteActive(Boolean(snapshot.active));
            if (!activeRequest.current) {
                setMessages(projected.messages);
                setTools(projected.tools);
                setDraft(projected.draft);
                setPartial('');
                const last = projected.messages.at(-1);
                if (snapshot.active) {
                    setStatus('modeling');
                    setError(null);
                } else if (snapshot.terminalError) {
                    setStatus('error');
                    setError(snapshot.terminalError.message);
                } else if (projected.unresolvedOutcome) {
                    setStatus('outcome_unknown');
                    setError(null);
                } else if (projected.permissionRequests.some((request) => request.state === 'pending')) {
                    setStatus('permission_required');
                    setError(null);
                } else if (projected.permissionRequests.some((request) =>
                    request.inputRevision === snapshot.inputRevision
                    && ['approved', 'partially_approved', 'denied'].includes(request.state),
                )) {
                    setStatus('done');
                    setError(null);
                } else if (last?.role === 'assistant' || last?.role === 'tool_result') {
                    setStatus('done');
                    setError(null);
                } else if (last?.role === 'user') {
                    setStatus('error');
                    // A snapshot has no terminal error payload; keep the more
                    // specific failure already received for this turn.
                    setError((current) => current ?? 'The AI Assistant turn ended before producing an answer.');
                } else {
                    setStatus('idle');
                    setError(null);
                }
            }
        } catch {
            // A transient poll failure must not erase the last durable view.
            if (reportFailure && snapshotEpoch.current === expectedEpoch && conversationId.current === expectedConversationId) {
                setError('history_restore_failed');
            }
        } finally {
            if (
                showHydrating
                && snapshotEpoch.current === expectedEpoch
                && conversationId.current === expectedConversationId
            ) {
                setHydrating(false);
            }
        }
    }, [deskId]);

    const loadOlderMessages = useCallback(async () => {
        const cursor = messagePage.nextBeforeMessageId;
        const expectedConversationId = conversationId.current;
        const watermark = snapshotWatermark.current;
        if (!messagePage.hasMore || !cursor || !expectedConversationId || !watermark || loadingOlderMessages) {
            return;
        }
        setLoadingOlderMessages(true);
        try {
            const response = await fetch(
                `/api/my/device-assistant-session?connection=${encodeURIComponent(deskId)}`
                + `&conversation=${encodeURIComponent(expectedConversationId)}`
                + `&message_before=${encodeURIComponent(cursor)}&message_limit=100`,
                { credentials: 'include', headers: { Accept: 'application/json' } },
            );
            const body = response.ok ? await response.json() : null;
            const snapshot = body?.data as PersistedSnapshot | undefined;
            if (
                !snapshot
                || conversationId.current !== expectedConversationId
                || snapshot.sessionId !== watermark.sessionId
                || snapshot.seq !== watermark.seq
                || !Array.isArray(snapshot.messages)
            ) return;
            const projected = projectPersistedSnapshot(snapshot);
            setMessages((current) => {
                const olderIds = new Set(projected.messages.map((message) => message.id));
                return [...projected.messages, ...current.filter((message) => !olderIds.has(message.id))];
            });
            setTools((current) => {
                let merged = projected.tools;
                for (const tool of current) merged = upsertTool(merged, tool);
                return merged;
            });
            setMessagePage({
                hasMore: Boolean(snapshot.messagePage?.hasMore),
                nextBeforeMessageId: snapshot.messagePage?.nextBeforeMessageId ?? null,
            });
        } finally {
            setLoadingOlderMessages(false);
        }
    }, [deskId, loadingOlderMessages, messagePage]);

    useEffect(() => {
        let stored: string | null = null;
        try {
            stored = localStorage.getItem(storageKey(conversationStorageScope));
        } catch {
            stored = null;
        }
        snapshotEpoch.current += 1;
        snapshotWatermark.current = null;
        conversationId.current = stored;
        setMessages([]);
        setTools([]);
        setDraft(null);
        setPartial('');
        setStatus('idle');
        setError(null);
        setAttachments([]);
        setVisualEvidence([]);
        setContextUsage(null);
        setContextNotices([]);
        setRemoteActive(false);
        activeRequest.current = null;
        contextRequest.current = null;
        if (contextTimer.current !== null) window.clearTimeout(contextTimer.current);
        contextTimer.current = null;
        setContextUpdating(false);
        setTaskStatusProjection(null);
        setPermissionRequests([]);
        setBackgroundTasks([]);
        setCapabilityGrants([]);
        setUnresolvedOutcome(null);
        setOutcomeDisposing(false);
        setPermissionUpdating(false);
        setGrantRevoking(null);
        setPendingInputCount(0);
        setMessagePage({ hasMore: false, nextBeforeMessageId: null });
        setLoadingOlderMessages(false);
        lastSeq.current = -1;
        previewArgs.current.clear();
        if (!stored) return;

        void loadSnapshot(stored, true);
    }, [conversationStorageScope, loadSnapshot]);

    useEffect(() => {
        const interval = window.setInterval(() => {
            if (conversationId.current) void loadSnapshot(conversationId.current);
        }, 2_000);
        return () => window.clearInterval(interval);
    }, [loadSnapshot]);

    useEffect(() => {
        const onStorage = (event: StorageEvent) => {
            if (
                event.key !== storageKey(conversationStorageScope)
                || event.newValue === conversationId.current
            ) {
                return;
            }
            activeRequest.current = null;
            contextRequest.current = null;
            if (contextTimer.current !== null) window.clearTimeout(contextTimer.current);
            contextTimer.current = null;
            setContextUpdating(false);
            setTaskStatusProjection(null);
            setPermissionRequests([]);
            setBackgroundTasks([]);
            setCapabilityGrants([]);
            setPermissionUpdating(false);
            setGrantRevoking(null);
            setPendingInputCount(0);
            snapshotEpoch.current += 1;
            snapshotWatermark.current = null;
            conversationId.current = event.newValue;
            lastSeq.current = -1;
            previewArgs.current.clear();
            setMessages([]);
            setTools([]);
            setDraft(null);
            setPartial('');
            setError(null);
            setAttachments([]);
            setVisualEvidence([]);
            setContextUsage(null);
            setContextNotices([]);
            setRemoteActive(false);
            setStatus('idle');
            if (event.newValue) void loadSnapshot(event.newValue, true);
        };
        window.addEventListener('storage', onStorage);
        return () => window.removeEventListener('storage', onStorage);
    }, [conversationStorageScope, loadSnapshot]);

    useEffect(() => () => {
        if (contextTimer.current !== null) window.clearTimeout(contextTimer.current);
    }, []);

    useEffect(() => subscribe((message: SignalingMessage) => {
        if (
            message.signaling_type === SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_SESSION_SELECTED
            && sessionTargetRequest.current
            && message.request_id === sessionTargetRequest.current
        ) {
            sessionTargetRequest.current = null;
            setSessionTargetResolving(false);
            const errorCode = message.response_state?.error_code;
            if (errorCode !== deskErrorCodeEnum.SUCCESS) {
                const list = parseSessionTargetList(message.signaling_data);
                setSessionTarget(null);
                setSessionTargetReady(false);
                setSessionTargets(list?.targets.filter((target) => target.assistant_ready) ?? []);
                if (!list?.targets.length) {
                    setError(message.response_state?.message ?? 'No AI Assistant desktop session is available.');
                }
                return;
            }
            const selected = message.signaling_data as {
                target?: SessionTargetDescriptor | null;
            } | null;
            setSessionTarget(selected?.target ?? null);
            setSessionTargets([]);
            setSessionTargetReady(true);
            return;
        }
        if (
            (
                message.signaling_type === SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_CONTEXT_UPDATED
                || message.signaling_type ===
                    SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_OBJECT_CONTEXT_UPDATED
            )
            && contextRequest.current
            && message.request_id === contextRequest.current
        ) {
            const ack = message.signaling_data as { error?: string | null };
            if (contextTimer.current !== null) window.clearTimeout(contextTimer.current);
            contextTimer.current = null;
            contextRequest.current = null;
            setContextUpdating(false);
            if (ack.error) setError(ack.error);
            if (conversationId.current) void loadSnapshot(conversationId.current);
            return;
        }
        if (message.signaling_type !== SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_UPDATED) return;
        if (!activeRequest.current || message.request_id !== activeRequest.current) return;
        const event = message.signaling_data as DeviceAssistantEvent;
        if (event.seq <= lastSeq.current) return;
        lastSeq.current = event.seq;

        switch (event.kind) {
            case 'status':
                setStatus(event.status ?? 'running');
                break;
            case 'partial':
                setPartial((current) => current + (event.partial_summary ?? ''));
                break;
            case 'partial_committed':
            case 'turn_started':
                break;
            case 'tool_started': {
                const callId = event.tool_call_id ?? `tool-${event.seq}`;
                const argumentsJson = event.tool_arguments_json ?? '{}';
                if (event.tool_name === PREVIEW_TOOL) {
                    previewArgs.current.set(callId, argumentsJson);
                }
                setTools((current) => upsertTool(current, {
                    callId,
                    name: event.tool_name ?? 'unknown',
                    status: 'running',
                    argumentsJson,
                    output: null,
                }));
                setStatus('using_tool');
                break;
            }
            case 'tool_finished': {
                const callId = event.tool_call_id ?? `tool-${event.seq}`;
                setTools((current) => {
                    const existing = current.find((tool) => tool.callId === callId);
                    return upsertTool(current, {
                        callId,
                        name: existing?.name ?? 'unknown',
                        status: event.tool_ok ? 'ok' : 'failed',
                        argumentsJson: existing?.argumentsJson ?? '{}',
                        output: event.tool_output ?? null,
                    });
                });
                if (event.tool_ok) {
                    const parsed = parseDraft(previewArgs.current.get(callId));
                    if (parsed) setDraft(parsed);
                }
                break;
            }
            case 'visual_evidence':
                if (event.visual_evidence) {
                    setVisualEvidence((current) => upsertVisualEvidence(current, event.visual_evidence!));
                }
                break;
            case 'answer':
                setMessages((current) => [...current, {
                    id: `assistant-${event.seq}`,
                    role: 'assistant',
                    text: event.answer ?? '',
                    provenance: event.provenance,
                }]);
                setPartial('');
                setStatus('done');
                activeRequest.current = null;
                setRemoteActive(false);
                if (conversationId.current) void loadSnapshot(conversationId.current);
                break;
            case 'permission_required':
                setPartial('');
                setStatus('permission_required');
                activeRequest.current = null;
                setRemoteActive(false);
                if (conversationId.current) void loadSnapshot(conversationId.current);
                break;
            case 'error':
            case 'retracted':
                setError(event.error?.message ?? 'The AI Assistant turn could not complete.');
                setPartial('');
                setStatus('error');
                activeRequest.current = null;
                setRemoteActive(false);
                if (conversationId.current) void loadSnapshot(conversationId.current);
                break;
        }
    }), [loadSnapshot, subscribe]);

    const ensureConversation = useCallback(() => {
        if (!conversationId.current) {
            snapshotEpoch.current += 1;
            snapshotWatermark.current = null;
            conversationId.current = v4();
            try {
                localStorage.setItem(
                    storageKey(conversationStorageScope),
                    conversationId.current,
                );
            } catch {
                // Conversation remains valid for this tab.
            }
        }
        return conversationId.current!;
    }, [conversationStorageScope]);

    const updateContext = useCallback((selectedCapabilityIds: string[]) => {
        if (activeRequest.current || remoteActive || contextRequest.current) return false;
        const currentConversationId = ensureConversation();
        const clientRequestId = v4();
        setContextUpdating(true);
        setError(null);
        contextRequest.current = sendMessage(
            SIGNALING_TYPE_CODE_UPDATE_DEVICE_ASSISTANT_CONTEXT,
            {
                conversation_id: currentConversationId,
                client_request_id: clientRequestId,
                selected_capability_ids: selectedCapabilityIds,
            },
            deskId,
        );
        contextTimer.current = window.setTimeout(() => {
            contextTimer.current = null;
            contextRequest.current = null;
            setContextUpdating(false);
            setError('AI Assistant context update timed out.');
            if (conversationId.current) void loadSnapshot(conversationId.current);
        }, 10_000);
        return true;
    }, [deskId, ensureConversation, loadSnapshot, remoteActive, sendMessage]);

    const detachAttachment = useCallback((attachmentId: string) => {
        if (activeRequest.current || remoteActive || contextRequest.current) return false;
        const currentConversationId = ensureConversation();
        const clientRequestId = v4();
        setContextUpdating(true);
        setError(null);
        contextRequest.current = sendMessage(
            SIGNALING_TYPE_CODE_UPDATE_DEVICE_ASSISTANT_OBJECT_CONTEXT,
            {
                conversation_id: currentConversationId,
                client_request_id: clientRequestId,
                operation: {
                    kind: 'detach',
                    attachment_id: attachmentId,
                },
            },
            deskId,
        );
        contextTimer.current = window.setTimeout(() => {
            contextTimer.current = null;
            contextRequest.current = null;
            setContextUpdating(false);
            setError('AI Assistant attachment update timed out.');
            if (conversationId.current) void loadSnapshot(conversationId.current);
        }, 10_000);
        return true;
    }, [deskId, ensureConversation, loadSnapshot, remoteActive, sendMessage]);

    const attachWindow = useCallback((objectRef: DeviceAssistantWindowRef, displaySummary: string) => {
        if (activeRequest.current || remoteActive || contextRequest.current) return false;
        const currentConversationId = ensureConversation();
        setContextUpdating(true);
        setError(null);
        contextRequest.current = sendMessage(
            SIGNALING_TYPE_CODE_UPDATE_DEVICE_ASSISTANT_OBJECT_CONTEXT,
            {
                conversation_id: currentConversationId,
                client_request_id: v4(),
                operation: {
                    kind: 'attach_window',
                    object_ref: objectRef,
                    display_summary: displaySummary,
                },
            },
            deskId,
        );
        contextTimer.current = window.setTimeout(() => {
            contextTimer.current = null;
            contextRequest.current = null;
            setContextUpdating(false);
            setError('AI Assistant window attachment timed out.');
            if (conversationId.current) void loadSnapshot(conversationId.current);
        }, 10_000);
        return true;
    }, [deskId, ensureConversation, loadSnapshot, remoteActive, sendMessage]);

    const start = useCallback((
        question: string,
        locale?: string,
        selectedCapabilityIds: string[] = [],
    ) => {
        const trimmed = question.trim();
        // A follow-up is durable input, not a second foreground workflow. Replace
        // the locally observed request stream with the newest request; the server
        // supersedes the older model turn under its input-revision fence.
        if (!trimmed || hydrating || contextRequest.current || !sessionTargetReady) return false;
        ensureConversation();
        const clientMessageId = `user-${v4()}`;
        setMessages((current) => [...current, {
            id: clientMessageId,
            role: 'user',
            text: trimmed,
        }]);
        setTools([]);
        setDraft(null);
        setPartial('');
        setError(null);
        setStatus('starting');
        setRemoteActive(true);
        lastSeq.current = -1;
        previewArgs.current.clear();
        activeRequest.current = sendMessage(
            SIGNALING_TYPE_CODE_ASK_DEVICE_ASSISTANT,
            {
                question: trimmed,
                client_message_id: clientMessageId,
                conversation_id: conversationId.current,
                locale,
                selected_capability_ids: selectedCapabilityIds,
                selected_attachment_ids: attachments
                    .filter((attachment) => attachment.state === 'active')
                    .map((attachment) => attachment.id),
            },
            deskId,
        );
        return true;
    }, [attachments, deskId, ensureConversation, sendMessage, sessionTargetReady, hydrating]);

    const submitPermissionDecision = useCallback(async (
        request: PermissionRequestDto,
        items: PermissionDecisionBody['items'],
    ) => {
        const currentConversationId = conversationId.current;
        if (!currentConversationId || request.state !== 'pending' || permissionUpdating) {
            return false;
        }
        const body: PermissionDecisionBody = {
            connection: deskId,
            conversation: currentConversationId,
            requestId: request.requestId,
            items,
        };
        setPermissionUpdating(true);
        setError(null);
        try {
            const response = await fetch('/api/my/device-assistant-session/permission-decision', {
                method: 'POST',
                credentials: 'include',
                headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
                body: JSON.stringify(body),
            });
            const result = response.ok ? await response.json() : null;
            if (!response.ok || !result?.success || !result?.data?.state) {
                throw new Error(result?.message ?? 'Permission decision was rejected.');
            }
            await loadSnapshot(currentConversationId);
            return true;
        } catch (reason) {
            setError(reason instanceof Error ? reason.message : 'Permission decision failed.');
            return false;
        } finally {
            setPermissionUpdating(false);
        }
    }, [deskId, loadSnapshot, permissionUpdating]);

    const decidePermission = useCallback((
        request: PermissionRequestDto,
        approve: boolean | readonly string[],
    ) => submitPermissionDecision(
        request,
        request.items.map((item) => (typeof approve === 'boolean'
            ? approve
            : approve.includes(item.itemId))
            ? {
                itemId: item.itemId,
                decision: 'approve',
                resource_scope: item.resourceScope,
                operation_scope: item.operationScope,
                export_destinations: item.exportDestinations,
                ttl_seconds: item.suggestedTtlSeconds,
                max_uses: item.suggestedMaxUses,
            }
            : { itemId: item.itemId, decision: 'deny' }),
    ), [submitPermissionDecision]);

    const decidePermissionItems = useCallback((
        request: PermissionRequestDto,
        items: PermissionDecisionBody['items'],
    ) => submitPermissionDecision(request, items), [submitPermissionDecision]);

    const revokeCapabilityGrant = useCallback(async (grantId: string) => {
        const currentConversationId = conversationId.current;
        if (!currentConversationId || grantRevoking) return false;
        setGrantRevoking(grantId);
        setError(null);
        try {
            const response = await fetch('/api/my/device-assistant-session/capability-grant/revoke', {
                method: 'POST',
                credentials: 'include',
                headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
                body: JSON.stringify({
                    connection: deskId,
                    conversation: currentConversationId,
                    grantId,
                    reason: 'revoked_by_owner',
                }),
            });
            const result = response.ok ? await response.json() : null;
            if (!response.ok || !result?.success || !result?.data?.grantId) {
                throw new Error(result?.message ?? 'Capability grant revocation was rejected.');
            }
            await loadSnapshot(currentConversationId);
            return true;
        } catch (reason) {
            setError(reason instanceof Error ? reason.message : 'Capability grant revocation failed.');
            return false;
        } finally {
            setGrantRevoking(null);
        }
    }, [deskId, grantRevoking, loadSnapshot]);

    const disposeUnknownOutcome = useCallback(async () => {
        const currentConversationId = conversationId.current;
        const outcome = unresolvedOutcome;
        if (!currentConversationId || !outcome || outcomeDisposing) return false;
        setOutcomeDisposing(true);
        setError(null);
        try {
            const response = await fetch('/api/my/device-assistant-session/outcome-unknown/dispose', {
                method: 'POST',
                credentials: 'include',
                headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
                body: JSON.stringify({
                    connection: deskId,
                    conversation: currentConversationId,
                    workId: outcome.workId,
                    executionId: outcome.executionId,
                }),
            });
            const result = response.ok ? await response.json() : null;
            if (!response.ok || !result?.success || result?.data?.disposed !== true) {
                throw new Error(result?.message ?? 'Unknown outcome disposition was rejected.');
            }
            await loadSnapshot(currentConversationId);
            return true;
        } catch (reason) {
            setError(reason instanceof Error ? reason.message : 'Unknown outcome disposition failed.');
            return false;
        } finally {
            setOutcomeDisposing(false);
        }
    }, [deskId, loadSnapshot, outcomeDisposing, unresolvedOutcome]);

    const reset = useCallback(() => {
        if (activeRequest.current) {
            sendMessage(
                SIGNALING_TYPE_CODE_CANCEL_DEVICE_ASSISTANT,
                null,
                deskId,
                activeRequest.current,
            );
        }
        activeRequest.current = null;
        contextRequest.current = null;
        if (contextTimer.current !== null) window.clearTimeout(contextTimer.current);
        contextTimer.current = null;
        snapshotEpoch.current += 1;
        snapshotWatermark.current = null;
        conversationId.current = null;
        lastSeq.current = -1;
        previewArgs.current.clear();
        setMessages([]);
        setTools([]);
        setDraft(null);
        setPartial('');
        setStatus('idle');
        setError(null);
        setAttachments([]);
        setVisualEvidence([]);
        setContextUsage(null);
        setContextNotices([]);
        setRemoteActive(false);
        setContextUpdating(false);
        setTaskStatusProjection(null);
        setPermissionRequests([]);
        setBackgroundTasks([]);
        setCapabilityGrants([]);
        setUnresolvedOutcome(null);
        setOutcomeDisposing(false);
        setPermissionUpdating(false);
        setGrantRevoking(null);
        setPendingInputCount(0);
        setMessagePage({ hasMore: false, nextBeforeMessageId: null });
        setLoadingOlderMessages(false);
        try {
            localStorage.removeItem(storageKey(conversationStorageScope));
        } catch {
            // Nothing else to clear.
        }
    }, [conversationStorageScope, deskId, sendMessage]);

    const selectConversation = useCallback((id: string) => {
        // Navigation must never cancel a turn or race an in-flight decision.
        if (!id || activeRequest.current || remoteActive || contextUpdating || permissionUpdating
            || outcomeDisposing || grantRevoking || hydrating) return false;
        reset();
        conversationId.current = id;
        try {
            localStorage.setItem(storageKey(conversationStorageScope), id);
        } catch { /* Continuation still works without local storage. */ }
        void loadSnapshot(id, true, true);
        return true;
    }, [remoteActive, contextUpdating, permissionUpdating, outcomeDisposing, grantRevoking,
        hydrating, reset, conversationStorageScope, loadSnapshot]);

    return {
        conversationId: conversationId.current,
        selectConversation,
        contextUsage,
        contextNotices,
        messages,
        tools,
        draft,
        partial,
        status,
        error,
        attachments,
        visualEvidence,
        hydrating,
        contextUpdating,
        taskStatusProjection,
        permissionRequests,
        backgroundTasks,
        capabilityGrants,
        unresolvedOutcome,
        outcomeDisposing,
        permissionUpdating,
        grantRevoking,
        pendingInputCount,
        hasMoreMessages: messagePage.hasMore,
        loadingOlderMessages,
        sessionTarget,
        sessionTargets,
        sessionTargetReady,
        sessionTargetResolving,
        running: activeRequest.current !== null || remoteActive || contextUpdating || permissionUpdating || outcomeDisposing,
        start,
        updateContext,
        attachWindow,
        detachAttachment,
        decidePermission,
        decidePermissionItems,
        revokeCapabilityGrant,
        disposeUnknownOutcome,
        loadOlderMessages,
        selectSessionTarget,
        reset,
    };
}
