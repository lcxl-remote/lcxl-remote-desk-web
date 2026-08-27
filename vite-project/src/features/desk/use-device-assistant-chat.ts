import { useCallback, useEffect, useRef, useState } from 'react';
import { v4 } from 'uuid';

import type { AiProvenance } from '@/components/ai-generated-mark';
import type {
    BackgroundTaskDto,
    CapabilityGrantDto,
    PermissionDecisionBody,
    PermissionRequestDto,
} from '@/services/types';
import type { DiagnoseEvent } from './diagnose-state';
import {
    SIGNALING_TYPE_CODE_ASK_DEVICE_ASSISTANT,
    SIGNALING_TYPE_CODE_CANCEL_DEVICE_ASSISTANT,
    SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_CONTEXT_UPDATED,
    SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_OBJECT_CONTEXT_UPDATED,
    SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_UPDATED,
    SIGNALING_TYPE_CODE_UPDATE_DEVICE_ASSISTANT_CONTEXT,
    SIGNALING_TYPE_CODE_UPDATE_DEVICE_ASSISTANT_OBJECT_CONTEXT,
} from './constants';
import type { SignalingMessage, SignalingSubscriber } from './use-desk-signaling';

const PREVIEW_TOOL = 'preview_computer_action';

export type DeviceAssistantMessage = {
    id: string;
    role: 'user' | 'assistant';
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

type PersistedSnapshotMessage = {
    id: string;
    role: string;
    text: string;
    toolCallId?: string | null;
    toolCalls?: PersistedToolCall[];
};

type PersistedSnapshot = {
    active: boolean;
    latestInputSeq?: number;
    inputRevision?: number;
    handledInputSeq?: number;
    taskStatusProjection?: DeviceAssistantTaskStatusProjection | null;
    permissionRequests?: PermissionRequestDto[];
    backgroundTasks?: BackgroundTaskDto[];
    capabilityGrants?: CapabilityGrantDto[];
    messages: PersistedSnapshotMessage[];
    contextAttachments?: DeviceAssistantContextAttachment[];
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
        if (message.role === 'tool' && message.toolCallId) {
            const existing = tools.find((tool) => tool.callId === message.toolCallId);
            tools = upsertTool(tools, {
                callId: message.toolCallId,
                name: existing?.name ?? 'unknown',
                status: /^(tool error:|not executed:)/i.test(message.text) ? 'failed' : 'ok',
                argumentsJson: existing?.argumentsJson ?? '{}',
                output: message.text || null,
            });
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
        pendingInputCount: Math.max(
            0,
            (snapshot.latestInputSeq ?? 0) - (snapshot.handledInputSeq ?? 0),
        ),
    };
}

export function useDeviceAssistantChat({
    deskId,
    conversationStorageScope = deskId,
    subscribe,
    sendMessage,
}: Props) {
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
    const [permissionUpdating, setPermissionUpdating] = useState(false);
    const [grantRevoking, setGrantRevoking] = useState<string | null>(null);
    const [pendingInputCount, setPendingInputCount] = useState(0);
    const activeRequest = useRef<string | null>(null);
    const contextRequest = useRef<string | null>(null);
    const contextTimer = useRef<number | null>(null);
    const conversationId = useRef<string | null>(null);
    const lastSeq = useRef(-1);
    const previewArgs = useRef(new Map<string, string>());

    const loadSnapshot = useCallback(async (
        expectedConversationId: string,
        showHydrating = false,
    ) => {
        if (showHydrating) setHydrating(true);
        try {
            const response = await fetch(
            `/api/my/diagnose-session?connection=${encodeURIComponent(deskId)}` +
                `&conversation=${encodeURIComponent(expectedConversationId)}`,
            { credentials: 'include', headers: { Accept: 'application/json' } },
            );
            const body = response.ok ? await response.json() : null;
            if (
                conversationId.current !== expectedConversationId
                || !Array.isArray(body?.data?.messages)
            ) return;
            const snapshot = body.data as PersistedSnapshot;
            const projected = projectPersistedSnapshot(snapshot);
            setAttachments(projected.attachments);
            setTaskStatusProjection(projected.taskStatusProjection);
            setPermissionRequests(projected.permissionRequests);
            setBackgroundTasks(projected.backgroundTasks);
            setCapabilityGrants(projected.capabilityGrants);
            setPendingInputCount(projected.pendingInputCount);
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
                } else if (projected.permissionRequests.some((request) => request.state === 'pending')) {
                    setStatus('permission_required');
                    setError(null);
                } else if (projected.permissionRequests.some((request) =>
                    request.inputRevision === snapshot.inputRevision
                    && ['approved', 'partially_approved', 'denied'].includes(request.state),
                )) {
                    setStatus('done');
                    setError(null);
                } else if (last?.role === 'assistant') {
                    setStatus('done');
                    setError(null);
                } else if (last?.role === 'user') {
                    setStatus('error');
                    setError('The Device Assistant turn ended before producing an answer.');
                } else {
                    setStatus('idle');
                    setError(null);
                }
            }
        } catch {
            // A transient poll failure must not erase the last durable view.
        } finally {
            if (showHydrating && conversationId.current === expectedConversationId) {
                setHydrating(false);
            }
        }
    }, [deskId]);

    useEffect(() => {
        let stored: string | null = null;
        try {
            stored = localStorage.getItem(storageKey(conversationStorageScope));
        } catch {
            stored = null;
        }
        conversationId.current = stored;
        setMessages([]);
        setTools([]);
        setDraft(null);
        setPartial('');
        setStatus('idle');
        setError(null);
        setAttachments([]);
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
        setPermissionUpdating(false);
        setGrantRevoking(null);
        setPendingInputCount(0);
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
            conversationId.current = event.newValue;
            lastSeq.current = -1;
            previewArgs.current.clear();
            setMessages([]);
            setTools([]);
            setDraft(null);
            setPartial('');
            setError(null);
            setAttachments([]);
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
        const event = message.signaling_data as DiagnoseEvent;
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
                setError(event.error?.message ?? 'The Device Assistant turn could not complete.');
                setPartial('');
                setStatus('error');
                activeRequest.current = null;
                setRemoteActive(false);
                if (conversationId.current) void loadSnapshot(conversationId.current);
                break;
            case 'final':
                // Device Assistant uses agentic `answer`, never Diagnose `final`.
                break;
        }
    }), [loadSnapshot, subscribe]);

    const ensureConversation = useCallback(() => {
        if (!conversationId.current) {
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
            setError('Device Assistant context update timed out.');
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
            setError('Device Assistant attachment update timed out.');
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
        if (!trimmed || contextRequest.current) return false;
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
    }, [attachments, deskId, ensureConversation, sendMessage]);

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
            const response = await fetch('/api/my/diagnose-session/permission-decision', {
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
            const response = await fetch('/api/my/diagnose-session/capability-grant/revoke', {
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
        setRemoteActive(false);
        setContextUpdating(false);
        setTaskStatusProjection(null);
        setPermissionRequests([]);
        setBackgroundTasks([]);
        setCapabilityGrants([]);
        setPermissionUpdating(false);
        setGrantRevoking(null);
        setPendingInputCount(0);
        try {
            localStorage.removeItem(storageKey(conversationStorageScope));
        } catch {
            // Nothing else to clear.
        }
    }, [conversationStorageScope, deskId, sendMessage]);

    return {
        messages,
        tools,
        draft,
        partial,
        status,
        error,
        attachments,
        hydrating,
        contextUpdating,
        taskStatusProjection,
        permissionRequests,
        backgroundTasks,
        capabilityGrants,
        permissionUpdating,
        grantRevoking,
        pendingInputCount,
        running: activeRequest.current !== null || remoteActive || contextUpdating || permissionUpdating,
        start,
        updateContext,
        detachAttachment,
        decidePermission,
        decidePermissionItems,
        revokeCapabilityGrant,
        reset,
    };
}
