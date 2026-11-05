import { mergeAssistantMessages } from './assistant-message-history';
import { projectGoalReviewReceipt } from './assistant-goal-review-receipt';
import type { AssistantContextUsage } from './assistant-context-meter';
import type { AssistantFileScopeView, AssistantDirectoryOperation } from './assistant-file-scope';
import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { agentErrorMessage } from '@/lib/agent-error-i18n';
import { v4 } from 'uuid';
import { assistantMessageId } from './assistant-native-ids';

import type { AiProvenance } from '@/components/ai-generated-mark';
import type {
    AiAssistantAsk,
    IssueSelection,
    AssistantCitation,
    BackgroundTaskDto,
    AiAssistantDelegationSnapshot,
    AiAssistantStopControl,
    CommandTaskDto,
    CapabilityGrantDto,
    ContextNoticeDto,
    PermissionDecisionBody,
    PermissionRequestDto,
    RehearsalView,
    AiAssistantMainRequestsDto,
} from '@/services/types';
import { deskErrorCodeEnum } from '@/services/types';
import type { AiAssistantDocumentPreview, AiAssistantDocumentPreviewPageResponse, AiAssistantEvent, AiAssistantVisualEvidence } from './ai-assistant-event';
import {
    SIGNALING_TYPE_CODE_ASK_AI_ASSISTANT,
    SIGNALING_TYPE_CODE_AI_ASSISTANT_CONTEXT_UPDATED,
    SIGNALING_TYPE_CODE_AI_ASSISTANT_OBJECT_CONTEXT_UPDATED,
    SIGNALING_TYPE_CODE_AI_ASSISTANT_SESSION_SELECTED,
    SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED,
    SIGNALING_TYPE_CODE_DOCUMENT_PREVIEW_PAGE_UPDATED,
    SIGNALING_TYPE_CODE_REQUEST_DOCUMENT_PREVIEW_PAGE,
    SIGNALING_TYPE_CODE_SELECT_AI_ASSISTANT_SESSION,
    SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_CONTEXT,
    SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_OBJECT_CONTEXT,
} from './constants';
import type { SignalingMessage, SignalingSubscriber } from './use-desk-signaling';
import {
    parseSessionTargetList,
    type SessionTargetDescriptor,
} from './session-target-selection';

const PREVIEW_TOOL = 'preview_computer_action';

const SNAPSHOT_TIMEOUT_MS = 30_000;
const REQUEST_QUEUE_PATH = '/api/my/ai-assistant-session/requests';

export type AiAssistantMessage = {
    citations?: AssistantCitation[];
    citationsIncomplete?: boolean;
    permissionReason?: string;
    contextBoundaryIds?: string[];
    id: string;
    role: 'user' | 'assistant' | 'tool_result' | 'tool_call' | 'goal_decision';
    goalDecision?: { status: 'approved' | 'denied' | 'expired'; reviewer: 'approval_ai' | 'system' };
    toolCallId?: string;
    turnId?: string | null;
    text: string;
    reasoning?: string | null;
    provenance?: AiProvenance | null;
};

export type AiAssistantToolActivity = {
    permissionReason?: string;
    callId: string;
    name: string;
    status: 'running' | 'ok' | 'failed' | 'returned';
    argumentsJson: string;
    output: string | null;
};

export type AiAssistantContextAttachment = {
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

export type AiAssistantWindowRef = {
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

export type RehearsalConversation = Pick<RehearsalView,
    'client_conversation_id' | 'initial_message_id' | 'prompt' | 'locale' | 'status'>;

type Props = {
    rehearsal?: RehearsalConversation;
    deskId: string;
    targetRefs?: readonly string[];
    connected?: boolean;
    /// Stable device identity for browser-side conversation intent. The OSS
    /// connection id changes after a server restart, while client_id does not.
    conversationStorageScope?: string;
    initialConversationId?: string | null;
    subscribe: (handler: SignalingSubscriber) => () => void;
    sendMessage: (
        type: number,
        data: unknown,
        connectionId?: string,
        requestId?: string,
    ) => string;
};

function storageKey(scope: string) {
    return `ai-assistant-conversation:${scope}`;
}

function sessionQuery(deskId: string, conversation: string) {
    const target = deskId ? `connection=${encodeURIComponent(deskId)}&` : '';
    return `/api/my/ai-assistant-session?${target}conversation=${encodeURIComponent(conversation)}`;
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
    tools: AiAssistantToolActivity[],
    next: AiAssistantToolActivity,
) {
    const index = tools.findIndex((tool) => tool.callId === next.callId);
    if (index === -1) return [...tools, next];
    const existing = tools[index];
    const merged = next.name === 'unknown' && existing.name !== 'unknown'
        ? { ...next, name: existing.name, argumentsJson: existing.argumentsJson } : next;
    return tools.map((tool, current) => current === index ? merged : tool);
}

function upsertVisualEvidence(
    current: AiAssistantVisualEvidence[],
    next: AiAssistantVisualEvidence,
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

export type AiAssistantTaskStatusProjection = {
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
    citations?: AssistantCitation[];
    citationsIncomplete?: boolean;
    reasoning?: string | null;
    id: string;
    turnId?: string | null;
    role: string;
    text: string;
    toolCallId?: string | null;
    toolOk?: boolean | null;
    backgroundTaskId?: string | null;
    toolCalls?: PersistedToolCall[];
};

export type AiAssistantGoal = {
    goalId: string;
    previousCompletedGoalId?: string | null;
    previousCompletionSummary?: string | null;
    goalRevision: number;
    goalText: string;
    deviceId: string;
    state: string;
    stateVersion: number;
    statusReason?: string | null;
    pauseReason?: string | null;
    sliceSeq: number;
    usedSlices: number;
    limitSlices: number | null;
    usedModelTokens: number;
    limitModelTokens: number | null;
    usedModelCalls: number;
    usedToolCalls: number;
    usedActiveTimeMs: number;
    limits: AiAssistantGoalLimits;
    createdAtUnixMs: number;
    deadlineUnixMs: number | null;
    nextAttemptUnixMs?: number | null;
    updatedAtUnixMs: number;
    checkpointSummary?: string | null;
};

export type AiAssistantGoalOpenRequest = {
    requestId: string;
    goalText: string;
    previousCompletedGoalId?: string | null;
    targetGoalId?: string | null;
    targetGoalRevision?: number | null;
    deviceId: string;
    inputRevision: number;
    limits: AiAssistantGoalLimits;
    expiresAtUnixMs: number;
};

export type AiAssistantGoalLimits = {
    activeTimeMs: number | null;
    modelTokens: number | null;
    modelCalls: number | null;
    toolCalls: number | null;
    slices: number | null;
    stalledSlices: number | null;
};

export type AiAssistantGoalOwnerAction = 'pause' | 'resume' | 'retry_stalled' | 'cancel';

export type AiAssistantApprovalDelegation = {
    delegationId: string;
    status: string;
    reviewsUsed: number;
    tokensUsed: number;
    costUsedMicros?: number | null;
    createdAtUnixMs: number;
};

export type AiAssistantApprovalModelReadiness = {
    available: boolean;
    reason?: string | null;
};

export type PersistedSnapshot = {
    temporaryConversation?: boolean | null;
    controlRevision: number;
    mainStopped: boolean;
    subagents: AiAssistantDelegationSnapshot;
    actionPermissionReasons?: Record<string, string>;
    requestId?: string;
    fileScope?: AssistantFileScopeView;
    terminalError?: { message: string; error_code?: number | null } | null;
    goal?: AiAssistantGoal | null;
    pendingGoalOpenRequest?: AiAssistantGoalOpenRequest | null;
    approvalDelegation?: AiAssistantApprovalDelegation | null;
    approvalModelReadiness?: AiAssistantApprovalModelReadiness | null;
    contextNotices?: ContextNoticeDto[];
    contextUsage?: AssistantContextUsage | null;
    sessionId: string;
    seq: number;
    active: boolean;
    latestInputSeq?: number;
    inputRevision?: number;
    handledInputSeq?: number;
    mainRequests?: AiAssistantMainRequestsDto['items'];
    taskStatusProjection?: AiAssistantTaskStatusProjection | null;
    permissionRequests?: PermissionRequestDto[];
    backgroundTasks?: BackgroundTaskDto[];
    commandTasks?: CommandTaskDto[];
    capabilityGrants?: CapabilityGrantDto[];
    messages: PersistedSnapshotMessage[];
    messagePage?: {
        hasMore: boolean;
        nextBeforeMessageId?: string | null;
        limit: number;
    };
    contextAttachments?: AiAssistantContextAttachment[];
    visualEvidence?: AiAssistantVisualEvidence[];
};

export function projectPersistedSnapshot(snapshot: Pick<PersistedSnapshot, 'messages' | 'actionPermissionReasons' | 'contextAttachments' | 'taskStatusProjection' | 'permissionRequests' | 'backgroundTasks' | 'capabilityGrants' | 'latestInputSeq' | 'handledInputSeq'>) {
    const messages: AiAssistantMessage[] = [];
    let tools: AiAssistantToolActivity[] = [];
    let draft: ComputerActionDraftPreview | null = null;
    for (const message of snapshot.messages) {
        const goalReceipt = projectGoalReviewReceipt(message);
        if (goalReceipt) messages.push(goalReceipt);
        if ((message.role === 'user' || message.role === 'assistant') && (message.text || (message.role === 'assistant' && message.reasoning))) {
            messages.push({
                id: message.id,
                role: message.role,
                text: message.text,
                turnId: message.turnId,
                citations: message.citations,
                citationsIncomplete: message.citationsIncomplete,
                reasoning: message.role === 'assistant' ? message.reasoning : undefined,
            });
        }
        for (const call of message.toolCalls ?? []) {
            messages.push({ id: `tool-call-${call.id}`, role: 'tool_call', toolCallId: call.id, turnId: message.turnId, text: '', contextBoundaryIds: [message.id] });
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
            if (!existing) {
                messages.push({ id: `tool-call-${message.toolCallId}`, role: 'tool_call', toolCallId: message.toolCallId, turnId: message.turnId, text: '', contextBoundaryIds: [message.id] });
            }
            let permissionReason: string | undefined;
            let nativeFailed = false;
            let nativeVerified = false;
            let nativeFileResult = false;
            try {
                const native = JSON.parse(message.text);
                permissionReason = snapshot.actionPermissionReasons?.[String(native.work_id)];
                nativeFailed = ['definitely_not_started', 'failed'].includes(native.result)
                    || ['failed', 'error'].includes(native.status)
                    || native.ok === false || native.success === false
                    || Boolean(native.error);
                nativeVerified = native.result === 'verified';
                nativeFileResult = ['file_artifact', 'batch_document_artifact', 'text_file_mutation'].includes(native.output?.kind);
            } catch { /* Non-native results have no work binding. */ }
            const backgroundRunning = /"status"\s*:\s*"background_running"/.test(message.text);
            tools = upsertTool(tools, {
                callId: message.toolCallId,
                permissionReason,
                name: existing?.name ?? 'unknown',
                status: backgroundRunning ? 'running'
                    : typeof message.toolOk === 'boolean' ? (message.toolOk ? 'ok' : 'failed')
                    : nativeFailed || /^(tool error:|not executed:|execution failed:|execution did not complete:|wait error:)/i.test(message.text.trimStart())
                        || (existing?.name === 'exec_command' && /^exit_code=(?!0(?:\n|$))\d+/i.test(message.text)) ? 'failed'
                        : nativeVerified || (existing?.name === 'exec_command' && /^exit_code=0(?:\n|$)/.test(message.text)) ? 'ok' : 'returned',
                argumentsJson: existing?.argumentsJson ?? '{}',
                output: message.text,
            });
            if (!backgroundRunning && message.text && (existing?.name === 'exec_command' || nativeFileResult || message.backgroundTaskId || (nativeFailed && permissionReason))) {
                messages.push({ id: message.id, role: 'tool_result', toolCallId: message.toolCallId, turnId: message.turnId, text: message.text, permissionReason });
            }
        }
    }
    let lastVisible: AiAssistantMessage | undefined;
    for (const raw of snapshot.messages) {
        lastVisible = messages.find(message => message.id === raw.id || message.contextBoundaryIds?.includes(raw.id)) ?? lastVisible;
        if (lastVisible && lastVisible.id !== raw.id) {
            lastVisible.contextBoundaryIds = [...new Set([...(lastVisible.contextBoundaryIds ?? []), raw.id])];
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

export function useAiAssistantChat({
    rehearsal,
    deskId,
    targetRefs,
    connected,
    conversationStorageScope = deskId,
    initialConversationId,
    subscribe,
    sendMessage,
}: Props) {
    const { t } = useTranslation();
    // Changing language must not reset the conversation or its snapshot requests.
    const translate = useRef(t);
    translate.current = t;
    const queueEnabled = !rehearsal;
    const targetSelectionEnabled = Boolean(deskId) && connected !== undefined;
    const temporaryConversationRef = useRef(false);
    const [temporaryConversation, setTemporaryConversation] = useState(false);
    const [contextUsage, setContextUsage] = useState<AssistantContextUsage | null>(null);
    const [contextNotices, setContextNotices] = useState<ContextNoticeDto[]>([]);
    const [messages, setMessages] = useState<AiAssistantMessage[]>([]);
    const [tools, setTools] = useState<AiAssistantToolActivity[]>([]);
    const [draft, setDraft] = useState<ComputerActionDraftPreview | null>(null);
    const [partial, setPartial] = useState('');
    const [status, setStatus] = useState('idle');
    const [error, setError] = useState<string | null>(null);
    const pendingDelivery = useRef<{
        id: string; question: string; conversation: string; requestId: string;
        payload: Record<string, unknown>; sentAt: number;
    } | null>(null);
    const deliveryFailure = useRef<string | null>(null);
    const [deliveryState, setDeliveryState] = useState<'sending' | 'unconfirmed' | null>(null);
    const [acceptedInput, setAcceptedInput] = useState<{ id: string; question: string } | null>(null);
    const acknowledgeDelivery = useCallback(() => {
        const pending = pendingDelivery.current;
        if (!pending) return;
        console.info('[assistant-input] accepted', { messageId: pending.id, requestId: pending.requestId });
        setAcceptedInput({ id: pending.id, question: pending.question });
        pendingDelivery.current = null;
        deliveryFailure.current = null;
        setDeliveryState(null);
    }, []);
    useEffect(() => {
        if (deliveryState !== 'sending') return;
        const timer = window.setInterval(() => {
            const pending = pendingDelivery.current;
            if (pending && Date.now() - pending.sentAt >= 15_000) {
                console.warn('[assistant-input] receipt timeout', { messageId: pending.id, requestId: pending.requestId });
                setDeliveryState('unconfirmed');
            }
        }, 1_000);
        return () => window.clearInterval(timer);
    }, [deliveryState]);
    const [attachments, setAttachments] = useState<AiAssistantContextAttachment[]>([]);
    const [fileScope, setFileScope] = useState<AssistantFileScopeView>({ revision: 0, directories: [] });
    const directorySelectors = useRef<{ conversationId: string; clientRequestId: string } | null>(null);
    const [hydrating, setHydrating] = useState(false);
    const [historyRestoreFailed, setHistoryRestoreFailed] = useState(false);
    const [remoteActive, setRemoteActive] = useState(false);
    const [contextUpdating, setContextUpdating] = useState(false);
    const [approvalInitializing, setApprovalInitializing] = useState(false);
    const [approvalInitializationFailed, setApprovalInitializationFailed] = useState(false);
    const approvalInitializationAttempt = useRef<string | null>(null);
    const approvalContextRequest = useRef<string | null>(null);
    const [taskStatusProjection, setTaskStatusProjection] =
        useState<AiAssistantTaskStatusProjection | null>(null);
    const [goal, setGoal] = useState<AiAssistantGoal | null>(null);
    const [goalUpdating, setGoalUpdating] = useState(false);
    const [pendingGoalOpenRequest, setPendingGoalOpenRequest] = useState<AiAssistantGoalOpenRequest | null>(null);
    const [goalOpenUpdating, setGoalOpenUpdating] = useState(false);
    const [approvalDelegation, setApprovalDelegation] = useState<AiAssistantApprovalDelegation | null>(null);
    const [approvalModelReadiness, setApprovalModelReadiness] = useState<AiAssistantApprovalModelReadiness | null>(null);
    const [approvalUpdating, setApprovalUpdating] = useState(false);
    const [permissionRequests, setPermissionRequests] = useState<PermissionRequestDto[]>([]);
    const [backgroundTasks, setBackgroundTasks] = useState<BackgroundTaskDto[]>([]);
    const [commandTasks, setCommandTasks] = useState<CommandTaskDto[]>([]);
    const [subagents, setSubagents] = useState<AiAssistantDelegationSnapshot>({ active_tasks: [], task: null, tasks: null, parent_session_id: null, attention_tasks: [], attention_count: 0 });
    const [mainStopped, setMainStopped] = useState(false);
    const [taskCancelling, setTaskCancelling] = useState<string | null>(null);
    const [capabilityGrants, setCapabilityGrants] = useState<CapabilityGrantDto[]>([]);
    const [outcomeDisposing, setOutcomeDisposing] = useState(false);
    const [permissionUpdating, setPermissionUpdating] = useState(false);
    const [browserConnectionId, setBrowserConnectionId] = useState<string | null>(null);
    const permissionSubmission = useRef<{ epoch: number } | null>(null);
    useEffect(() => {
        permissionSubmission.current = null;
        setPermissionUpdating(false);
        return () => { permissionSubmission.current = null; };
    }, [deskId, connected]);
    const [grantRevoking, setGrantRevoking] = useState<string | null>(null);
    const [pendingInputCount, setPendingInputCount] = useState(0);
    const [requestQueue, setRequestQueue] = useState<(AiAssistantMainRequestsDto & { conversation: string }) | null>(null);
    const [resumingQueue, setResumingQueue] = useState(false);
    const queueReadOrder = useRef(0);
    const queuedSession = useRef<{ conversation: string; sessionId: string } | null>(null);
    const queueStreamIds = useRef(new Set<string>());
    const rememberQueueStream = useCallback((id: string) => {
        const ids = queueStreamIds.current;
        ids.add(id);
        if (ids.size > 64) {
            const oldest = ids.values().next().value;
            if (oldest !== undefined) ids.delete(oldest);
        }
    }, []);
    const [messagePage, setMessagePage] = useState<{
        hasMore: boolean;
        nextBeforeMessageId: string | null;
    }>({ hasMore: false, nextBeforeMessageId: null });
    const [loadingOlderMessages, setLoadingOlderMessages] = useState(false);
    const [visualEvidence, setVisualEvidence] = useState<AiAssistantVisualEvidence[]>([]);
    const [documentPreviews, setDocumentPreviews] = useState<AiAssistantDocumentPreview[]>([]);
    const documentPreviewPageRequests = useRef(new Map<string, { previewId: string; page: number }>());
    const [sessionTarget, setSessionTarget] = useState<SessionTargetDescriptor | null>(null);
    const [sessionTargets, setSessionTargets] = useState<SessionTargetDescriptor[]>([]);
    const [sessionTargetReady, setSessionTargetReady] = useState(!targetSelectionEnabled);
    const [sessionTargetResolving, setSessionTargetResolving] = useState(false);
    const activeRequest = useRef<string | null>(null);
    const snapshotActiveRequest = useRef<string | null>(null);
    const [stopping, setStopping] = useState(false);
    const [stopConfirmation, setStopConfirmation] = useState<{
        session: string; conversation: string; inputRevision: number; controlRevision: number;
    } | null>(null);
    const stopPending = useRef(false);
    const stopOperation = useRef<object | null>(null);
    const clearStopping = useCallback((force = false) => {
        if (stopOperation.current && !force) return;
        if (force) stopOperation.current = null;
        stopPending.current = false;
        setStopping(false);
    }, []);
    const contextRequest = useRef<string | null>(null);
    const contextTimer = useRef<number | null>(null);
    const conversationId = useRef<string | null>(null);
    const rehearsalSent = useRef(false);
    const snapshotEpoch = useRef(0);
    const snapshotRequestOrder = useRef(0);
    /**
     * The local request the server has acknowledged, with the snapshot order
     * current at that moment. Only a snapshot requested after the
     * acknowledgement can prove that request ended; an older one may predate it.
     */
    const activeRequestAck = useRef<{ requestId: string; order: number } | null>(null);
    const stoppingRequest = useRef<string | null>(null);
    const acknowledgeActiveRequest = (requestId: string) => {
        if (activeRequestAck.current?.requestId !== requestId) {
            activeRequestAck.current = { requestId, order: snapshotRequestOrder.current };
        }
    };
    const snapshotAcceptanceFloor = useRef(0);
    const snapshotWatermark = useRef<{
        conversationId: string;
        sessionId: string;
        seq: number;
        requestOrder: number;
        inputRevision?: number;
        controlRevision: number;
    } | null>(null);
    // Retain only durable snapshots, not optimistic/streaming messages. Once
    // history is expanded, tail polling must not collapse the loaded window.
    const historyWindow = useRef<{
        conversationId: string;
        sessionId: string;
        expanded: boolean;
        messages: AiAssistantMessage[];
        tools: AiAssistantToolActivity[];
        page: { hasMore: boolean; nextBeforeMessageId: string | null };
    } | null>(null);
    const olderRequest = useRef<object | null>(null);
    const lastSeq = useRef(-1);
    const currentTurnId = useRef<string | null>(null);
    const previewArgs = useRef(new Map<string, string>());
    const sessionTargetRequest = useRef<string | null>(null);

    const observeQueue = useCallback((conversation: string, queue: AiAssistantMainRequestsDto) => {
        queuedSession.current = { conversation, sessionId: queue.sessionId };
        setRequestQueue({ ...queue, conversation });
        for (const item of queue.items) {
            if (item.state === 'queued' || item.state === 'activated') rememberQueueStream(item.requestId);
            else queueStreamIds.current.delete(item.requestId);
        }
        const pending = pendingDelivery.current;
        if (pending?.conversation === conversation
            && queue.items.some(item => item.clientMessageId === pending.id)
            && !pending.payload.start_goal) {
            acknowledgeDelivery();
        }
    }, [acknowledgeDelivery, rememberQueueStream]);

    const loadRequestQueue = useCallback(async (expectedConversation: string) => {
        if (!queueEnabled) return;
        const order = ++queueReadOrder.current;
        const epoch = snapshotEpoch.current;
        const saved = queuedSession.current;
        const query = saved?.conversation === expectedConversation
            ? new URLSearchParams({ session: saved.sessionId })
            : new URLSearchParams(deskId
                ? { connection: deskId, conversation: expectedConversation }
                : { conversation: expectedConversation });
        try {
            const response = await fetch(`${REQUEST_QUEUE_PATH}?${query}`, {
                credentials: 'include', headers: { Accept: 'application/json' },
                signal: AbortSignal.timeout(SNAPSHOT_TIMEOUT_MS),
            });
            const queue = response.ok ? (await response.json())?.data as AiAssistantMainRequestsDto | undefined : undefined;
            if (conversationId.current !== expectedConversation || epoch !== snapshotEpoch.current
                || order !== queueReadOrder.current || !Array.isArray(queue?.items)
                || typeof queue.sessionId !== 'string') return;
            observeQueue(expectedConversation, queue);
        } catch { /* Keep the last authoritative queue view on a failed read. */ }
    }, [deskId, observeQueue, queueEnabled]);

    const cancelQueuedRequest = useCallback(async (requestId: string) => {
        const saved = queuedSession.current;
        if (!saved || conversationId.current !== saved.conversation) return;
        try {
            const response = await fetch(`${REQUEST_QUEUE_PATH}/cancel`, {
                method: 'POST', credentials: 'include',
                headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
                body: JSON.stringify({ session: saved.sessionId, requestId }),
                signal: AbortSignal.timeout(SNAPSHOT_TIMEOUT_MS),
            });
            const body = response.ok ? await response.json() : null;
            if (conversationId.current !== saved.conversation) return;
            if (body?.success !== true) {
                setError(agentErrorMessage(translate.current, body?.code, body?.message,
                    'The AI Assistant request could not be cancelled.'));
            }
        } catch {
            if (conversationId.current === saved.conversation) setError('The AI Assistant request could not be cancelled.');
        }
        if (conversationId.current === saved.conversation) await loadRequestQueue(saved.conversation);
    }, [loadRequestQueue]);

    const selectSessionTarget = useCallback((targetId?: string) => {
        if (!targetSelectionEnabled || !connected || sessionTargetRequest.current) return false;
        setSessionTargetResolving(true);
        setError(null);
        sessionTargetRequest.current = sendMessage(
            SIGNALING_TYPE_CODE_SELECT_AI_ASSISTANT_SESSION,
            targetId ? { session_target_id: targetId } : {},
            deskId,
        );
        return true;
    }, [connected, deskId, sendMessage, targetSelectionEnabled]);

    const requestDocumentPreviewPage = useCallback((previewId: string, page: number) => {
        const sessionId = snapshotWatermark.current?.conversationId === conversationId.current
            ? snapshotWatermark.current.sessionId : undefined;
        const preview = documentPreviews.find(item => item.descriptor.preview_id === previewId);
        if (!deskId || connected === false || !sessionId || !preview || preview.loading
            || !Number.isInteger(page) || page < 1 || page > preview.descriptor.page_count) return false;
        const requestId = sendMessage(
            SIGNALING_TYPE_CODE_REQUEST_DOCUMENT_PREVIEW_PAGE,
            { conversation_id: sessionId, preview_id: previewId, page, spec_version: 1 },
            deskId,
        );
        documentPreviewPageRequests.current.set(requestId, { previewId, page });
        setDocumentPreviews(current => current.map(item => item.descriptor.preview_id === previewId
            ? { ...item, loading: true, error: null } : item));
        return true;
    }, [connected, deskId, documentPreviews, sendMessage]);

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
        requireApprovalState = false,
    ): Promise<'ready' | 'missing' | 'ignored' | 'failed'> => {
        const expectedEpoch = snapshotEpoch.current;
        const expectedRequestOrder = ++snapshotRequestOrder.current;
        const expectedQueueOrder = queueReadOrder.current;
        if (showHydrating) setHydrating(true);
        const failed = () => {
            if ((reportFailure || showHydrating) && snapshotEpoch.current === expectedEpoch
                && conversationId.current === expectedConversationId
                && expectedRequestOrder >= snapshotAcceptanceFloor.current
                && expectedRequestOrder >= (snapshotWatermark.current?.requestOrder ?? 0)) {
                setHistoryRestoreFailed(true);
            }
            return 'failed' as const;
        };
        const controller = new AbortController();
        const timeout = window.setTimeout(() => controller.abort(), SNAPSHOT_TIMEOUT_MS);
        try {
            const response = await fetch(
                sessionQuery(deskId, expectedConversationId),
            { credentials: 'include', headers: { Accept: 'application/json' }, signal: controller.signal },
            );
            const body = response.ok ? await response.json() : null;
            if (
                snapshotEpoch.current !== expectedEpoch
                || expectedRequestOrder < snapshotAcceptanceFloor.current
                || conversationId.current !== expectedConversationId
            ) return 'ignored';
            if (!Array.isArray(body?.data?.messages)) {
                const missing = response.ok && body?.success === false
                    && body?.code === deskErrorCodeEnum.PERMISSION_ERROR && body.data == null;
                return missing && !reportFailure ? 'missing' : failed();
            }
            const snapshot = body.data as PersistedSnapshot;
            const pending = pendingDelivery.current;
            if (pending?.conversation === expectedConversationId
                && snapshot.messages.some(message => message.id === pending.id && message.role === 'user')) {
                if (queueEnabled && !pending.payload.start_goal && snapshot.requestId !== pending.requestId
                    && activeRequest.current === pending.requestId) {
                    activeRequest.current = null;
                    activeRequestAck.current = null;
                }
                acknowledgeDelivery();
            }
            if (
                typeof snapshot.sessionId !== 'string'
                || snapshot.sessionId.length === 0
                || !Number.isSafeInteger(snapshot.seq)
                || snapshot.seq < 0
                || !Number.isSafeInteger(snapshot.controlRevision) || snapshot.controlRevision < 1
                || typeof snapshot.mainStopped !== 'boolean'
                || !snapshot.subagents || !Array.isArray(snapshot.subagents.active_tasks)
                || !Array.isArray(snapshot.subagents.attention_tasks) || !Number.isSafeInteger(snapshot.subagents.attention_count)
                || snapshot.subagents.attention_count < 0
                || snapshot.subagents.task !== null || snapshot.subagents.parent_session_id !== null
                || (requireApprovalState && typeof snapshot.approvalModelReadiness?.available !== 'boolean')
            ) return failed();
            const watermark = snapshotWatermark.current;
            if (
                watermark
                && (
                    watermark.conversationId !== expectedConversationId
                    || (
                        watermark.sessionId === snapshot.sessionId
                        && (snapshot.seq < watermark.seq || (snapshot.seq === watermark.seq
                            && expectedRequestOrder <= watermark.requestOrder))
                    )
                    || (
                        watermark.sessionId !== snapshot.sessionId
                        && expectedRequestOrder <= watermark.requestOrder
                    )
                )
            ) return 'ignored';
            if (typeof snapshot.temporaryConversation === 'boolean') {
                temporaryConversationRef.current = snapshot.temporaryConversation;
                setTemporaryConversation(snapshot.temporaryConversation);
            }
            snapshotWatermark.current = {
                conversationId: expectedConversationId,
                sessionId: snapshot.sessionId,
                seq: snapshot.seq,
                requestOrder: expectedRequestOrder,
                inputRevision: snapshot.inputRevision,
                controlRevision: snapshot.controlRevision,
            };
            if (queueEnabled && expectedQueueOrder === queueReadOrder.current && Array.isArray(snapshot.mainRequests)) {
                queueReadOrder.current += 1;
                observeQueue(expectedConversationId, { sessionId: snapshot.sessionId, items: snapshot.mainRequests });
            }
            setHistoryRestoreFailed(false);
            setStopConfirmation(current => current && (current.session !== snapshot.sessionId
                || current.inputRevision !== snapshot.inputRevision || current.controlRevision !== snapshot.controlRevision)
                ? null : current);
            snapshotActiveRequest.current = snapshot.active ? snapshot.requestId ?? null : null;
            const local = activeRequest.current;
            if (local && snapshot.active && snapshot.requestId === local) acknowledgeActiveRequest(local);
            const ack = activeRequestAck.current;
            if (local && ack?.requestId === local && expectedRequestOrder > ack.order
                && (!snapshot.active || snapshot.requestId !== local)) {
                // A snapshot newer than the acknowledgement shows the local
                // request is over (its terminal event was lost). Stop targeting
                // it: the server's active request, if any, is the current one.
                activeRequest.current = null;
                activeRequestAck.current = null;
                if (stoppingRequest.current === local) clearStopping();
            }
            if (!snapshot.active) {
                if (!stopPending.current) clearStopping();
                // An expired server lease also settles a locally bound request
                // whose terminal event was lost during a disconnect or restart.
                if (activeRequest.current === snapshot.requestId) activeRequest.current = null;
            }
            setContextUsage(snapshot.contextUsage ?? null);
            setContextNotices([...new Map((snapshot.contextNotices ?? []).map(notice => [notice.id, notice])).values()]);
            const projected = projectPersistedSnapshot(snapshot);
            const previous = historyWindow.current;
            const expanded = previous?.conversationId === expectedConversationId
                && previous.sessionId === snapshot.sessionId && previous.expanded;
            const windowMessages = expanded
                ? mergeAssistantMessages(previous.messages, projected.messages) : projected.messages;
            const windowTools = expanded
                ? projected.tools.reduce((items, tool) => upsertTool(items, tool), previous.tools) : projected.tools;
            const page = expanded ? previous.page : {
                hasMore: Boolean(snapshot.messagePage?.hasMore),
                nextBeforeMessageId: snapshot.messagePage?.nextBeforeMessageId ?? null,
            };
            historyWindow.current = {
                conversationId: expectedConversationId, sessionId: snapshot.sessionId,
                expanded: Boolean(expanded), messages: windowMessages, tools: windowTools, page,
            };

            setAttachments(projected.attachments);
            setFileScope(snapshot.fileScope ?? { revision: 0, directories: [] });
            setTaskStatusProjection(projected.taskStatusProjection);
            setGoal(snapshot.goal ?? null);
            setPendingGoalOpenRequest(snapshot.pendingGoalOpenRequest ?? null);
            setApprovalDelegation(snapshot.approvalDelegation ?? null);
            setApprovalModelReadiness(snapshot.approvalModelReadiness ?? null);
            if (typeof snapshot.approvalModelReadiness?.available === 'boolean') setApprovalInitializationFailed(false);
            setPermissionRequests(projected.permissionRequests);
            setBackgroundTasks(projected.backgroundTasks);
            setCommandTasks(snapshot.commandTasks ?? []);
            setSubagents(snapshot.subagents);
            setMainStopped(snapshot.mainStopped);
            setCapabilityGrants(projected.capabilityGrants);
            setPendingInputCount(projected.pendingInputCount);
            setMessagePage(page);
            setVisualEvidence((current) => (snapshot.visualEvidence ?? []).reduce(
                (items, next) => upsertVisualEvidence(items, next),
                [] as AiAssistantVisualEvidence[],
            ).map((next) => {
                const live = current.find((item) => item.evidence_id === next.evidence_id);
                return live?.preview_data_url && !next.preview_data_url
                    ? { ...next, status: live.status, preview_data_url: live.preview_data_url }
                    : next;
            }));
            setRemoteActive(Boolean(snapshot.active));
            if (!activeRequest.current) {
                const pending = pendingDelivery.current;
                setMessages(pending && pending.conversation === expectedConversationId && !windowMessages.some(m => m.id === pending.id)
                    ? [...windowMessages, { id: pending.id, role: 'user', text: pending.question }] : windowMessages);
                setTools(windowTools);
                setDraft(projected.draft);
                setPartial('');
                const last = projected.messages.at(-1);
                if (deliveryFailure.current) {
                    setStatus('error');
                    setError(deliveryFailure.current);
                } else if (snapshot.active) {
                    setStatus('modeling');
                    setError(null);
                } else if (snapshot.terminalError) {
                    setStatus('error');
                    setError(agentErrorMessage(translate.current, snapshot.terminalError.error_code,
                        snapshot.terminalError.message, 'The AI Assistant turn could not complete.'));

                } else if (snapshot.mainStopped) {
                    setStatus('cancelled');
                    setError(null);
                } else if (snapshot.goal && !['completed', 'failed', 'cancelled'].includes(snapshot.goal.state)) {
                    setStatus(snapshot.goal.state === 'waiting_approval' ? 'permission_required' : 'done');
                    setError(null);
                } else if (projected.permissionRequests.some((request) => request.state === 'pending')
                    || snapshot.fileScope?.directories.some(directory => directory.state === 'pending')) {
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
            return 'ready';
        } catch {
            // A transient poll failure must not erase the last durable view.
            return failed();
        } finally {
            window.clearTimeout(timeout);
            if (
                showHydrating
                && snapshotEpoch.current === expectedEpoch
                && conversationId.current === expectedConversationId
            ) {
                setHydrating(false);
            }
        }
    }, [acknowledgeDelivery, deskId, observeQueue, queueEnabled]);

    const loadOlderMessages = useCallback(async () => {
        const cursor = messagePage.nextBeforeMessageId;
        const expectedConversationId = conversationId.current;
        const watermark = snapshotWatermark.current;
        if (!messagePage.hasMore || !cursor || !expectedConversationId || !watermark || olderRequest.current) {
            return;
        }
        const expectedEpoch = snapshotEpoch.current;
        const request = {};
        olderRequest.current = request;
        setLoadingOlderMessages(true);
        try {
            const response = await fetch(
                sessionQuery(deskId, expectedConversationId)
                + `&message_before=${encodeURIComponent(cursor)}&message_limit=100`,
                { credentials: 'include', headers: { Accept: 'application/json' } },
            );
            const body = response.ok ? await response.json() : null;
            const snapshot = body?.data as PersistedSnapshot | undefined;
            if (
                !snapshot
                || snapshotEpoch.current !== expectedEpoch
                || olderRequest.current !== request
                || snapshotWatermark.current?.sessionId !== watermark.sessionId
                || conversationId.current !== expectedConversationId
                || snapshot.sessionId !== watermark.sessionId
                || !Number.isSafeInteger(snapshot.seq) || snapshot.seq < watermark.seq
                || !Array.isArray(snapshot.messages)
            ) return;
            const projected = projectPersistedSnapshot(snapshot);
            const current = historyWindow.current;
            if (!current || current.conversationId !== expectedConversationId
                || current.sessionId !== snapshot.sessionId) return;
            const page = {
                hasMore: Boolean(snapshot.messagePage?.hasMore),
                nextBeforeMessageId: snapshot.messagePage?.nextBeforeMessageId ?? null,
            };
            historyWindow.current = {
                ...current, expanded: true, page,
                messages: mergeAssistantMessages(projected.messages, current.messages),
                tools: current.tools.reduce((items, tool) => upsertTool(items, tool), projected.tools),
            };
            setMessages((messages) => mergeAssistantMessages(projected.messages, messages));
            setTools((tools) => tools.reduce((items, tool) => upsertTool(items, tool), projected.tools));
            setMessagePage(page);
        } finally {
            if (olderRequest.current === request) {
                olderRequest.current = null;
                setLoadingOlderMessages(false);
            }
        }
    }, [deskId, messagePage]);

    useEffect(() => {
        let stored: string | null = null;
        try {
            stored = rehearsal?.client_conversation_id ?? initialConversationId
                ?? localStorage.getItem(storageKey(conversationStorageScope));
        } catch {
            stored = null;
        }
        snapshotEpoch.current += 1;
        snapshotWatermark.current = null;
        queuedSession.current = null;
        queueStreamIds.current.clear();
        setRequestQueue(null);
        historyWindow.current = null;
        olderRequest.current = null;
        pendingDelivery.current = null;
        deliveryFailure.current = null;
        setDeliveryState(null);
        setAcceptedInput(null);
        conversationId.current = stored;
        temporaryConversationRef.current = false;
        setTemporaryConversation(false);
        currentTurnId.current = null;
        rehearsalSent.current = false;
        setMessages([]);
        setTools([]);
        setDraft(null);
        setPartial('');
        setStatus('idle');
        setError(null);
        setHistoryRestoreFailed(false);
        setAttachments([]);
        setFileScope({ revision: 0, directories: [] });
        directorySelectors.current = null;
        setVisualEvidence([]);
        setContextUsage(null);
        setContextNotices([]);
        setRemoteActive(false);
        activeRequest.current = null;
        snapshotActiveRequest.current = null;
        clearStopping(true);
        contextRequest.current = null;
        if (contextTimer.current !== null) window.clearTimeout(contextTimer.current);
        contextTimer.current = null;
        setContextUpdating(false);
        setTaskStatusProjection(null);
        setGoal(null);
        setPendingGoalOpenRequest(null);
        setApprovalDelegation(null);
        setApprovalModelReadiness(null);
        setApprovalInitializing(false);
        setApprovalInitializationFailed(false);
        approvalInitializationAttempt.current = null;
        approvalContextRequest.current = null;
        setPermissionRequests([]);
        setBackgroundTasks([]);
        setCommandTasks([]);
        setSubagents({ active_tasks: [], task: null, tasks: null, parent_session_id: null, attention_tasks: [], attention_count: 0 });
        setMainStopped(false);
        setStopConfirmation(null);
        setCapabilityGrants([]);
        setOutcomeDisposing(false);
        setPermissionUpdating(false);
        setGrantRevoking(null);
        setPendingInputCount(0);
        setMessagePage({ hasMore: false, nextBeforeMessageId: null });
        setLoadingOlderMessages(false);
        lastSeq.current = -1;
        previewArgs.current.clear();
        if (!stored) return;

        void loadSnapshot(stored, true).then(outcome => {
            if (outcome === 'missing') return loadRequestQueue(stored);
        });
    }, [conversationStorageScope, initialConversationId, loadRequestQueue, loadSnapshot, rehearsal?.client_conversation_id]);

    useEffect(() => {
        // A slow snapshot must not stack further requests behind it.
        let polling = false;
        const poll = () => {
            if (!conversationId.current || polling) return;
            polling = true;
            const selected = conversationId.current;
            void loadSnapshot(selected).then(outcome => {
                if (outcome === 'missing' || !snapshotWatermark.current) return loadRequestQueue(selected);
            })
                .finally(() => { polling = false; });
        };
        const interval = window.setInterval(poll, 2_000);
        return () => window.clearInterval(interval);
    }, [loadRequestQueue, loadSnapshot]);

    useEffect(() => {
        const onStorage = (event: StorageEvent) => {
            if (rehearsal) return;
            if (
                event.key !== storageKey(conversationStorageScope)
                || event.newValue === conversationId.current
            ) {
                return;
            }
            activeRequest.current = null;
            snapshotActiveRequest.current = null;
            clearStopping(true);
            contextRequest.current = null;
            if (contextTimer.current !== null) window.clearTimeout(contextTimer.current);
            contextTimer.current = null;
            setContextUpdating(false);
            setTaskStatusProjection(null);
            setGoal(null);
            setPendingGoalOpenRequest(null);
            setApprovalDelegation(null);
            setApprovalModelReadiness(null);
            setApprovalInitializing(false);
            setApprovalInitializationFailed(false);
            approvalInitializationAttempt.current = null;
            approvalContextRequest.current = null;
            setPermissionRequests([]);
            setBackgroundTasks([]);
            setCommandTasks([]);
            setSubagents({ active_tasks: [], task: null, tasks: null, parent_session_id: null, attention_tasks: [], attention_count: 0 });
            setMainStopped(false);
            setStopConfirmation(null);
            setCapabilityGrants([]);
            setPermissionUpdating(false);
            setGrantRevoking(null);
            setPendingInputCount(0);
            snapshotEpoch.current += 1;
            snapshotWatermark.current = null;
            queuedSession.current = null;
            queueStreamIds.current.clear();
            setRequestQueue(null);
            historyWindow.current = null;
            olderRequest.current = null;
            setMessagePage({ hasMore: false, nextBeforeMessageId: null });
            setLoadingOlderMessages(false);
            conversationId.current = event.newValue;
            temporaryConversationRef.current = false;
            setTemporaryConversation(false);
            currentTurnId.current = null;
            lastSeq.current = -1;
            previewArgs.current.clear();
            setMessages([]);
            setTools([]);
            setDraft(null);
            setPartial('');
            setError(null);
            setHistoryRestoreFailed(false);
            setAttachments([]);
            setFileScope({ revision: 0, directories: [] });
            directorySelectors.current = null;
            setVisualEvidence([]);
            setDocumentPreviews([]);
            documentPreviewPageRequests.current.clear();
            setContextUsage(null);
            setContextNotices([]);
            setRemoteActive(false);
            setStatus('idle');
            if (event.newValue) {
                const selected = event.newValue;
                void loadSnapshot(selected, true).then(outcome => {
                    if (outcome === 'missing') return loadRequestQueue(selected);
                });
            }
        };
        window.addEventListener('storage', onStorage);
        return () => window.removeEventListener('storage', onStorage);
    }, [conversationStorageScope, loadRequestQueue, loadSnapshot, rehearsal]);

    useEffect(() => () => {
        if (contextTimer.current !== null) window.clearTimeout(contextTimer.current);
    }, []);

    useEffect(() => subscribe((message: SignalingMessage) => {
        // Inbound frames name this client's own signaling connection; an
        // interactive command's terminal carrier is bound to it.
        if (message.to_connection_id) setBrowserConnectionId(message.to_connection_id);
        if (message.signaling_type === SIGNALING_TYPE_CODE_DOCUMENT_PREVIEW_PAGE_UPDATED) {
            if (!message.request_id) return;
            const pending = documentPreviewPageRequests.current.get(message.request_id);
            if (!pending) return;
            documentPreviewPageRequests.current.delete(message.request_id);
            const response = message.signaling_data as AiAssistantDocumentPreviewPageResponse | null;
            setDocumentPreviews(current => current.map(item => {
                if (item.descriptor.preview_id !== pending.previewId) return item;
                if (!response || response.preview_id !== pending.previewId
                    || response.page !== pending.page || !response.frame) {
                    return { ...item, loading: false,
                        error: response?.error ?? translate.current('pages.aiAssistant.documentPreview.loadFailed') };
                }
                return { ...item, page: response.frame.page,
                    preview_data_url: response.frame.preview_data_url, loading: false, error: null };
            }));
            return;
        }
        if (
            message.signaling_type === SIGNALING_TYPE_CODE_AI_ASSISTANT_SESSION_SELECTED
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
                message.signaling_type === SIGNALING_TYPE_CODE_AI_ASSISTANT_CONTEXT_UPDATED
                || message.signaling_type ===
                    SIGNALING_TYPE_CODE_AI_ASSISTANT_OBJECT_CONTEXT_UPDATED
            )
            && contextRequest.current
            && message.request_id === contextRequest.current
        ) {
            const ack = message.signaling_data as { error?: string | null; conversation_id?: string; client_request_id?: string };
            if (directorySelectors.current && (
                message.signaling_type !== SIGNALING_TYPE_CODE_AI_ASSISTANT_OBJECT_CONTEXT_UPDATED
                || ack?.conversation_id !== directorySelectors.current.conversationId
                || ack?.client_request_id !== directorySelectors.current.clientRequestId
            )) return;
            directorySelectors.current = null;
            if (contextTimer.current !== null) window.clearTimeout(contextTimer.current);
            contextTimer.current = null;
            contextRequest.current = null;
            setContextUpdating(false);
            if (ack.error) setError(ack.error);
            const initializingApproval = approvalContextRequest.current === message.request_id;
            if (initializingApproval) approvalContextRequest.current = null;
            if (conversationId.current) {
                const selected = conversationId.current;
                void loadSnapshot(selected, false, false, initializingApproval).then(outcome => {
                    if (initializingApproval && conversationId.current === selected && outcome !== 'ignored') {
                        setApprovalInitializationFailed(outcome !== 'ready' || Boolean(ack.error));
                    }
                });
            }
            return;
        }
        if (message.signaling_type !== SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED) return;
        const event = message.signaling_data as AiAssistantEvent;
        if (!event || message.request_id !== event.request_id) return;
        const pending = pendingDelivery.current;
        const currentSubmission = pending?.requestId === event.request_id;
        if (event.kind === 'status' && event.status === 'queued' && currentSubmission
            && event.main_request_id && event.main_request_session_id && conversationId.current) {
            const selected = conversationId.current;
            queuedSession.current = { conversation: selected, sessionId: event.main_request_session_id };
            rememberQueueStream(event.main_request_id);
            acknowledgeDelivery();
            if (!activeRequest.current && !snapshotActiveRequest.current) setStatus('queued');
            void loadRequestQueue(selected);
            void loadSnapshot(selected);
            return;
        }
        if (currentSubmission && !pending.payload.start_goal && !rehearsal
            && event.kind === 'error' && event.seq <= 1 && !stopPending.current) {
            const failure = agentErrorMessage(translate.current, event.error?.error_code,
                event.error?.message, 'The AI Assistant turn could not complete.');
            deliveryFailure.current = failure;
            pendingDelivery.current = null;
            setDeliveryState(null);
            setMessages(current => current.filter(message => message.id !== pending.id));
            setError(failure);
            if (!activeRequest.current && !snapshotActiveRequest.current) setStatus('error');
            if (conversationId.current) void loadSnapshot(conversationId.current);
            return;
        }
        const beginsQueuedStream = queueStreamIds.current.has(event.request_id)
            && (event.kind === 'turn_started' || (event.kind === 'status' && event.status === 'accepted'));
        if (!activeRequest.current && (currentSubmission || beginsQueuedStream)) {
            activeRequest.current = event.request_id;
            lastSeq.current = -1;
            currentTurnId.current = null;
            setPartial('');
            setRemoteActive(true);
        }
        if (message.request_id !== activeRequest.current) return;
        if (event.seq <= lastSeq.current) return;
        lastSeq.current = event.seq;
        if (event.status !== 'accepting') acknowledgeActiveRequest(message.request_id);

        if (event.kind === 'status' && event.status === 'accepted' && currentSubmission) acknowledgeDelivery();
        if (event.kind === 'error' && event.seq <= 1 && currentSubmission && !stopPending.current) {
            deliveryFailure.current = agentErrorMessage(translate.current, event.error?.error_code,
                event.error?.message, 'The AI Assistant turn could not complete.');
            const rejectedId = pending.id;
            pendingDelivery.current = null;
            setDeliveryState(null);
            setMessages(current => current.filter(message => message.id !== rejectedId));
        }
        if ((event.kind === 'error' || event.kind === 'retracted') && currentSubmission && pendingDelivery.current) setDeliveryState('unconfirmed');
        switch (event.kind) {
            case 'status':
                setStatus(event.status ?? 'running');
                break;
            case 'partial':
                setPartial((current) => current + (event.partial_summary ?? ''));
                break;
            case 'partial_committed':
                break;
            case 'turn_started':
                currentTurnId.current = event.turn_id ?? null;
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
                setMessages(current => current.some(message => message.toolCallId === callId) ? current : [...current, {
                    id: `tool-call-${callId}`, role: 'tool_call', toolCallId: callId, turnId: currentTurnId.current, text: '',
                }]);
                setStatus('using_tool');
                break;
            }
            case 'tool_finished': {
                const callId = event.tool_call_id ?? `tool-${event.seq}`;
                setMessages(current => current.some(message => message.toolCallId === callId) ? current : [...current, {
                    id: `tool-call-${callId}`, role: 'tool_call', toolCallId: callId, turnId: currentTurnId.current, text: '',
                }]);
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
            case 'document_preview':
                if (event.document_preview) {
                    setDocumentPreviews(current => [
                        ...current.filter(preview => preview.descriptor.preview_id
                            !== event.document_preview!.descriptor.preview_id),
                        event.document_preview!,
                    ]);
                }
                break;
            case 'answer':
                queueStreamIds.current.delete(event.request_id);
                setMessages((current) => [...current, {
                    id: `assistant-${event.request_id}-${event.seq}`,
                    role: 'assistant',
                    text: event.answer ?? '',
                    provenance: event.provenance,
                }]);
                setPartial('');
                setStatus('done');
                activeRequest.current = null;
                clearStopping();
                setRemoteActive(false);
                if (conversationId.current) void loadSnapshot(conversationId.current);
                break;
            case 'permission_required':
                queueStreamIds.current.delete(event.request_id);
                setPartial('');
                setStatus('permission_required');
                activeRequest.current = null;
                clearStopping();
                setRemoteActive(false);
                if (conversationId.current) void loadSnapshot(conversationId.current);
                break;
            case 'error':
            case 'retracted':
                queueStreamIds.current.delete(event.request_id);
                setError(agentErrorMessage(translate.current, event.error?.error_code,
                    event.error?.message, 'The AI Assistant turn could not complete.'));
                setPartial('');
                setStatus('error');
                activeRequest.current = null;
                clearStopping();
                setRemoteActive(false);
                if (conversationId.current) void loadSnapshot(conversationId.current);
                break;
        }
    }), [acknowledgeDelivery, loadRequestQueue, loadSnapshot, rehearsal, rememberQueueStream, subscribe]);

    const ensureConversation = useCallback(() => {
        if (!conversationId.current) {
            snapshotEpoch.current += 1;
            snapshotWatermark.current = null;
            historyWindow.current = null;
            olderRequest.current = null;
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
        if (rehearsal && (rehearsal.status === 'completed' || rehearsal.status === 'cancelled' || rehearsal.status === 'failed' || (!rehearsalSent.current && rehearsal.status !== 'running'))) return false;
        if (activeRequest.current || remoteActive || contextRequest.current) return false;
        const currentConversationId = ensureConversation();
        const clientRequestId = v4();
        setContextUpdating(true);
        setError(null);
        contextRequest.current = sendMessage(
            SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_CONTEXT,
            {
                conversation_id: currentConversationId,
                client_request_id: clientRequestId,
                selected_capability_ids: [...selectedCapabilityIds],
            },
            deskId,
        );
        contextTimer.current = window.setTimeout(() => {
            contextTimer.current = null;
            if (approvalContextRequest.current !== null && approvalContextRequest.current === contextRequest.current) {
                approvalContextRequest.current = null;
                setApprovalInitializationFailed(true);
            }
            contextRequest.current = null;
            setContextUpdating(false);
            setError('AI Assistant context update timed out.');
            if (conversationId.current) void loadSnapshot(conversationId.current);
        }, 10_000);
        return true;
    }, [deskId, ensureConversation, loadSnapshot, remoteActive, sendMessage, rehearsal]);

    const initializeApprovalConversation = useCallback(async (retry = false) => {
        if (rehearsal || connected === false || hydrating || approvalInitializing
            || activeRequest.current || remoteActive || contextRequest.current) return false;
        const selected = ensureConversation();
        if (snapshotWatermark.current?.conversationId === selected && approvalModelReadiness) return true;
        if (!retry && approvalInitializationAttempt.current === selected) return false;
        approvalInitializationAttempt.current = selected;
        setApprovalInitializing(true);
        setApprovalInitializationFailed(false);
        setError(null);
        try {
            // A client UUID alone is not a persisted conversation. Confirm its
            // absence before sending an empty selection that could clear context.
            const outcome = await loadSnapshot(selected, false, false, true);
            if (conversationId.current !== selected || outcome === 'ignored') return false;
            if (outcome === 'ready') return true;
            if (outcome === 'missing' && updateContext([])) {
                approvalContextRequest.current = contextRequest.current;
                return true;
            }
            setApprovalInitializationFailed(true);
            return false;
        } catch {
            if (conversationId.current === selected) setApprovalInitializationFailed(true);
            return false;
        } finally {
            if (conversationId.current === selected) setApprovalInitializing(false);
        }
    }, [rehearsal, connected, hydrating, approvalInitializing, remoteActive,
        ensureConversation, approvalModelReadiness, loadSnapshot, updateContext]);

    const detachAttachment = useCallback((attachmentId: string) => {
        if (rehearsal && (rehearsal.status === 'completed' || rehearsal.status === 'cancelled' || rehearsal.status === 'failed' || (!rehearsalSent.current && rehearsal.status !== 'running'))) return false;
        if (activeRequest.current || remoteActive || contextRequest.current) return false;
        const currentConversationId = ensureConversation();
        const clientRequestId = v4();
        setContextUpdating(true);
        setError(null);
        contextRequest.current = sendMessage(
            SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_OBJECT_CONTEXT,
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
    }, [deskId, ensureConversation, loadSnapshot, remoteActive, sendMessage, rehearsal]);

    const updateDirectory = useCallback((operation: AssistantDirectoryOperation, timeoutMessage: string) => {
        if (rehearsal && (rehearsal.status === 'completed' || rehearsal.status === 'cancelled' || rehearsal.status === 'failed' || (!rehearsalSent.current && rehearsal.status !== 'running'))) return false;
        if (contextRequest.current || !connected || hydrating) return false;
        const currentConversationId = ensureConversation();
        const clientRequestId = v4();
        directorySelectors.current = { conversationId: currentConversationId, clientRequestId };
        setContextUpdating(true);
        setError(null);
        contextRequest.current = sendMessage(SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_OBJECT_CONTEXT, {
            conversation_id: currentConversationId, client_request_id: clientRequestId, operation,
        }, deskId);
        contextTimer.current = window.setTimeout(() => {
            directorySelectors.current = null;
            contextTimer.current = null;
            contextRequest.current = null;
            setContextUpdating(false);
            setError(timeoutMessage);
            if (conversationId.current) void loadSnapshot(conversationId.current);
        }, 35_000);
        return true;
    }, [connected, deskId, ensureConversation, hydrating, loadSnapshot, sendMessage, rehearsal]);

    const attachWindow = useCallback((objectRef: AiAssistantWindowRef, displaySummary: string) => {
        if (rehearsal && (rehearsal.status === 'completed' || rehearsal.status === 'cancelled' || rehearsal.status === 'failed' || (!rehearsalSent.current && rehearsal.status !== 'running'))) return false;
        if (activeRequest.current || remoteActive || contextRequest.current) return false;
        const currentConversationId = ensureConversation();
        setContextUpdating(true);
        setError(null);
        contextRequest.current = sendMessage(
            SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_OBJECT_CONTEXT,
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
    }, [deskId, ensureConversation, loadSnapshot, remoteActive, sendMessage, rehearsal]);

    const start = useCallback((
        question: string,
        locale?: string,
        selectedCapabilityIds: string[] = [],
        startGoal = false,
        previousCompletedGoalId: string | null = null,
        selectedIssues: IssueSelection[] = [],
    ) => {
        if (rehearsal && (rehearsal.status !== 'pending' || rehearsalSent.current || question !== rehearsal.prompt || conversationId.current !== rehearsal.client_conversation_id)) return false;
        const trimmed = rehearsal ? rehearsal.prompt : question.trim();
        if (!trimmed || hydrating || historyRestoreFailed || approvalInitializing || contextRequest.current
            || !sessionTargetReady || pendingDelivery.current || stopPending.current) return false;
        const nativeTurn = Boolean(rehearsal || (deskId && startGoal));
        if (nativeTurn && (activeRequest.current || remoteActive)) return false;
        const selectedObjects = attachments.filter(attachment => attachment.state === 'active' && attachment.kind !== 'interactive_session');
        if (selectedObjects.some(attachment => attachment.expiresAtUnixMs <= Date.now())) {
            deliveryFailure.current = 'selected_context_expired';
            setError(deliveryFailure.current);
            return false;
        }
        deliveryFailure.current = null;
        setStopConfirmation(null);
        snapshotEpoch.current += 1;
        clearStopping(true);
        ensureConversation();
        const clientMessageId = rehearsal?.initial_message_id ?? assistantMessageId();
        if (rehearsal) rehearsalSent.current = true;
        setMessages((current) => [...current, {
            id: clientMessageId,
            role: 'user',
            text: trimmed,
        }]);
        if (nativeTurn) {
            setTools([]);
            setDraft(null);
            setPartial('');
            setRemoteActive(true);
            previewArgs.current.clear();
            documentPreviewPageRequests.current.clear();
        }
        setError(null);
        if (nativeTurn || (!activeRequest.current && !snapshotActiveRequest.current)) setStatus('starting');
        if (nativeTurn) lastSeq.current = -1;
        const requestId = v4();
        const payload = {
                question: trimmed,
                temporary_conversation: rehearsal || !deskId ? undefined : temporaryConversationRef.current,
                client_message_id: clientMessageId,
                client_timezone: Intl.DateTimeFormat().resolvedOptions().timeZone,
                conversation_id: conversationId.current,
                locale: rehearsal ? rehearsal.locale ?? undefined : locale,
                ...(!deskId && targetRefs?.length ? { target_refs: [...targetRefs] } : {}),
                selected_capability_ids: deskId ? [...selectedCapabilityIds] : [],
                start_goal: Boolean(deskId) && !rehearsal && startGoal,
                previous_completed_goal_id: deskId && !rehearsal && startGoal ? previousCompletedGoalId : null,
                selected_attachment_ids: rehearsal || !deskId ? [] : selectedObjects
                    .map((attachment) => attachment.id),
                ...(!rehearsal && selectedIssues.length ? { selected_issues: selectedIssues.map(issue => ({ ...issue })) } : {}),
            } satisfies AiAssistantAsk;
        pendingDelivery.current = { id: clientMessageId, question: trimmed,
            conversation: conversationId.current!, requestId, payload, sentAt: Date.now() };
        setDeliveryState('sending');
        if (nativeTurn) activeRequest.current = requestId;
        console.info('[assistant-input] dispatch', { messageId: clientMessageId, requestId });
        try {
            const sent = sendMessage(SIGNALING_TYPE_CODE_ASK_AI_ASSISTANT, payload, deskId || undefined, requestId);
            if (nativeTurn) activeRequest.current = sent;
            if (pendingDelivery.current) pendingDelivery.current.requestId = sent;
        } catch {
            setDeliveryState('unconfirmed');
        }
        return true;
    }, [attachments, deskId, targetRefs, ensureConversation, sendMessage, sessionTargetReady, hydrating, historyRestoreFailed, approvalInitializing, rehearsal, remoteActive]);

    const retryDelivery = useCallback(async () => {
        const pending = pendingDelivery.current;
        if (!pending || connected === false || !sessionTargetReady || deliveryState === 'sending') return;
        setDeliveryState('sending');
        pending.sentAt = Date.now();
        await loadSnapshot(pending.conversation);
        if (pendingDelivery.current !== pending || conversationId.current !== pending.conversation) return;
        if (rehearsal || pending.payload.start_goal) {
            activeRequest.current = pending.requestId;
            lastSeq.current = -1;
        }
        console.info('[assistant-input] retry', { messageId: pending.id, requestId: pending.requestId });
        try {
            sendMessage(SIGNALING_TYPE_CODE_ASK_AI_ASSISTANT, pending.payload, deskId || undefined, pending.requestId);
        } catch { setDeliveryState('unconfirmed'); }
    }, [connected, sessionTargetReady, deliveryState, loadSnapshot, sendMessage, deskId, rehearsal]);

    const submitPermissionDecision = useCallback(async (
        request: PermissionRequestDto,
        items: PermissionDecisionBody['items'],
        carrierId?: string,
    ) => {
        const currentConversationId = conversationId.current;
        if (!currentConversationId || connected === false || hydrating || remoteActive || activeRequest.current
            || request.state !== 'pending' || permissionUpdating
            || request.inputRevision !== snapshotWatermark.current?.inputRevision
            || permissionSubmission.current?.epoch === snapshotEpoch.current
            || !permissionRequests.some(current => current.requestId === request.requestId
                && current.inputRevision === request.inputRevision && current.state === 'pending')) {
            return false;
        }
        const submission = { epoch: snapshotEpoch.current };
        permissionSubmission.current = submission;
        const isCurrent = () => permissionSubmission.current === submission
            && snapshotEpoch.current === submission.epoch && conversationId.current === currentConversationId;
        const body: PermissionDecisionBody = {
            connection: deskId || undefined,
            conversation: currentConversationId,
            requestId: request.requestId,
            items,
            ...(carrierId ? { carrierId } : {}),
        };
        setPermissionUpdating(true);
        setError(null);
        try {
            const response = await fetch('/api/my/ai-assistant-session/permission-decision', {
                method: 'POST',
                credentials: 'include',
                headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
                body: JSON.stringify(body),
            });
            const result = response.ok ? await response.json() : null;
            if (!isCurrent()) return false;
            if (!response.ok || !result?.success || !result?.data?.state) {
                throw new Error(result?.message ?? 'Permission decision was rejected.');
            }
            await loadSnapshot(currentConversationId);
            return isCurrent();
        } catch (reason) {
            if (!isCurrent()) return false;
            // An uncertain response is not a failed decision. Read durable state
            // once; never automatically repeat the approval mutation.
            await loadSnapshot(currentConversationId);
            if (!isCurrent()) return false;
            setError(reason instanceof Error ? reason.message : 'Permission decision failed.');
            return false;
        } finally {
            if (isCurrent()) {
                permissionSubmission.current = null;
                setPermissionUpdating(false);
            }
        }
    }, [deskId, loadSnapshot, permissionUpdating, permissionRequests, connected, hydrating, remoteActive]);

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
        carrierId?: string,
    ) => submitPermissionDecision(request, items, carrierId), [submitPermissionDecision]);

    const setAutomaticApproval = useCallback(async (enabled: boolean) => {
        const selected = conversationId.current;
        const watermark = snapshotWatermark.current;
        if (!selected || !watermark || watermark.conversationId !== selected
            || !Number.isSafeInteger(watermark.inputRevision)
            || approvalUpdating || (enabled && (!approvalModelReadiness?.available
                || remoteActive || activeRequest.current !== null))
            || (!enabled && !approvalDelegation)) return false;
        setApprovalUpdating(true);
        setError(null);
        const endpoint = enabled ? 'open' : 'close';
        const payload = enabled ? {
            connection: deskId,
            conversation: selected,
            session: watermark.sessionId,
            expectedInputRevision: watermark.inputRevision,
            ownerAuthorizationId: v4(),
        } : {
            connection: deskId,
            conversation: selected,
            session: watermark.sessionId,
            delegationId: approvalDelegation?.delegationId,
            ownerDecisionId: v4(),
        };
        try {
            const response = await fetch(`/api/my/ai-assistant-session/approval-delegation/${endpoint}`, {
                method: 'POST', credentials: 'include',
                headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
                body: JSON.stringify(payload),
            });
            const result = response.ok ? await response.json() : null;
            if (!response.ok || !result?.success || !result?.data?.delegationId) {
                throw new Error(result?.message ?? 'Approval mode change was rejected.');
            }
            if (conversationId.current === selected) await loadSnapshot(selected);
            return true;
        } catch (reason) {
            // A lost response may still have committed the owner decision.
            if (conversationId.current === selected) {
                await loadSnapshot(selected);
                setError(reason instanceof Error ? reason.message : 'Approval mode change failed.');
            }
            return false;
        } finally {
            setApprovalUpdating(false);
        }
    }, [deskId, approvalUpdating, approvalModelReadiness, approvalDelegation,
        remoteActive, loadSnapshot]);

    const controlGoal = useCallback(async (action: AiAssistantGoalOwnerAction) => {
        const selected = conversationId.current;
        const watermark = snapshotWatermark.current;
        if (!selected || !watermark || watermark.conversationId !== selected
            || !goal || !Number.isSafeInteger(goal.stateVersion) || goalUpdating) return false;
        setGoalUpdating(true);
        setError(null);
        try {
            const response = await fetch('/api/my/ai-assistant-session/goal/control', {
                method: 'POST', credentials: 'include',
                headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
                body: JSON.stringify({
                    connection: deskId, conversation: selected, session: watermark.sessionId,
                    goalId: goal.goalId, expectedStateVersion: goal.stateVersion,
                    action,
                }),
            });
            const result = response.ok ? await response.json() : null;
            if (!response.ok || !result?.success || !result?.data?.goalId) {
                throw new Error(result?.message ?? 'Goal control was rejected.');
            }
            if (conversationId.current === selected) {
                snapshotAcceptanceFloor.current = ++snapshotRequestOrder.current;
                setGoal(result.data as AiAssistantGoal);
                await loadSnapshot(selected);
            }
            return true;
        } catch (reason) {
            if (conversationId.current === selected) {
                await loadSnapshot(selected);
                setError(reason instanceof Error ? reason.message : 'Goal control failed.');
            }
            return false;
        } finally {
            setGoalUpdating(false);
        }
    }, [deskId, goal, goalUpdating, loadSnapshot]);

    const decideGoalOpen = useCallback(async (approve: boolean) => {
        const selected = conversationId.current;
        const watermark = snapshotWatermark.current;
        const request = pendingGoalOpenRequest;
        if (!selected || !watermark || watermark.conversationId !== selected
            || !request || goalOpenUpdating || request.expiresAtUnixMs <= Date.now()) return false;
        setGoalOpenUpdating(true);
        setError(null);
        try {
            const response = await fetch('/api/my/ai-assistant-session/goal/open-decision', {
                method: 'POST', credentials: 'include',
                headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
                body: JSON.stringify({
                    connection: deskId, conversation: selected, session: watermark.sessionId,
                    requestId: request.requestId, approve,
                }),
            });
            const result = response.ok ? await response.json() : null;
            if (!response.ok || !result?.success || !result?.data?.requestId) {
                throw new Error(result?.message ?? 'Goal decision was rejected.');
            }
            if (conversationId.current === selected) {
                snapshotAcceptanceFloor.current = ++snapshotRequestOrder.current;
                setPendingGoalOpenRequest(null);
                if (result.data.goal) setGoal(result.data.goal as AiAssistantGoal);
                await loadSnapshot(selected);
            }
            return true;
        } catch (reason) {
            if (conversationId.current === selected) {
                await loadSnapshot(selected);
                setError(reason instanceof Error ? reason.message : 'Goal decision failed.');
            }
            return false;
        } finally {
            setGoalOpenUpdating(false);
        }
    }, [deskId, pendingGoalOpenRequest, goalOpenUpdating, loadSnapshot]);

    const revokeCapabilityGrant = useCallback(async (grantId: string) => {
        const currentConversationId = conversationId.current;
        if (!currentConversationId || grantRevoking) return false;
        setGrantRevoking(grantId);
        setError(null);
        try {
            const response = await fetch('/api/my/ai-assistant-session/capability-grant/revoke', {
                method: 'POST',
                credentials: 'include',
                headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
                body: JSON.stringify({
                    connection: deskId || undefined,
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

    const cancelTask = useCallback(async (kind: 'command' | 'provider', taskId: string): Promise<void> => {
        if (!conversationId.current || taskCancelling) throw new Error('Task cancellation is unavailable.');
        const currentConversationId = conversationId.current;
        setTaskCancelling(`${kind}:${taskId}`);
        try {
            if (kind === 'command') {
                const task = commandTasks.find(item => item.taskId === taskId);
                const session = snapshotWatermark.current?.conversationId === currentConversationId
                    ? snapshotWatermark.current.sessionId : null;
                if (!session || !task || !['running', 'outcome_unknown'].includes(task.state)) {
                    throw new Error('Task is no longer cancellable.');
                }
                const response = await fetch('/api/my/ai-assistant-session/command/cancel', {
                    method: 'POST', credentials: 'include', headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ connection: deskId, session, exec_request_id: task.taskId,
                    execution_generation: task.executionGeneration,
                    }),
                });
                const body = await response.json();
                if (!response.ok || body.code !== deskErrorCodeEnum.SUCCESS || typeof body.data !== 'boolean')
                    throw new Error(body.message || 'Cancellation failed.');
            } else {
                const task = backgroundTasks.find(item => item.taskId === taskId);
                if (!task?.supportsCancel || !['running', 'outcome_unknown'].includes(task.state)) {
                    throw new Error('Task is no longer cancellable.');
                }
                const response = await fetch('/api/my/ai-assistant-session/background-task/cancel', {
                    method: 'POST', credentials: 'include', headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ connection: deskId, conversation: currentConversationId,
                        taskId, requestId: v4(), reason: 'Cancelled by the conversation owner.' }),
                });
                const body = await response.json();
                if (!response.ok || body.code !== deskErrorCodeEnum.SUCCESS) throw new Error(body.message || 'Cancellation failed.');
            }
            if (conversationId.current === currentConversationId) await loadSnapshot(currentConversationId);
        } finally {
            setTaskCancelling(null);
        }
    }, [backgroundTasks, commandTasks, deskId, loadSnapshot, taskCancelling]);

    const submitStop = useCallback(async (selection: NonNullable<typeof stopConfirmation>,
        choice: AiAssistantStopControl['subagent_choice']) => {
        if (stopPending.current || conversationId.current !== selection.conversation) return false;
        const epoch = snapshotEpoch.current;
        const operation = {};
        stopOperation.current = operation;
        const isCurrent = () => stopOperation.current === operation && snapshotEpoch.current === epoch
            && conversationId.current === selection.conversation;
        const control: AiAssistantStopControl = {
            client_request_id: v4(), expected_input_revision: selection.inputRevision,
            expected_control_revision: selection.controlRevision, subagent_choice: choice,
        };
        stopPending.current = true;
        stoppingRequest.current = activeRequest.current ?? snapshotActiveRequest.current;
        setStopping(true);
        const controller = new AbortController();
        const timer = window.setTimeout(() => controller.abort(), 15_000);
        try {
            const response = await fetch('/api/my/ai-assistant-session/stop', {
                method: 'POST', credentials: 'include', signal: controller.signal,
                headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
                body: JSON.stringify({ connection: deskId || undefined, conversation: selection.conversation,
                    session: selection.session, control }),
            });
            const result = await response.json();
            if (!isCurrent()) return false;
            if (!response.ok || result.code !== deskErrorCodeEnum.SUCCESS || !result.data) {
                throw new Error(result.message || translate.current('pages.aiAssistant.stopFailed'));
            }
            activeRequest.current = null;
            snapshotActiveRequest.current = null;
            activeRequestAck.current = null;
            setRemoteActive(false);
            setMainStopped(true);
            setPartial('');
            setStatus('cancelled');
            setStopConfirmation(null);
            await loadSnapshot(selection.conversation);
            return true;
        } catch (reason) {
            if (!isCurrent()) return false;
            // A lost reply may follow a committed stop. Refresh facts once;
            // never automatically repeat an owner control mutation.
            await loadSnapshot(selection.conversation);
            if (isCurrent()) setError(reason instanceof Error ? reason.message
                : translate.current('pages.aiAssistant.stopFailed'));
            return false;
        } finally {
            window.clearTimeout(timer);
            if (isCurrent()) clearStopping(true);
        }
    }, [deskId, loadSnapshot, clearStopping]);

    const stop = useCallback(() => {
        const watermark = snapshotWatermark.current;
        const selected = conversationId.current;
        if (!selected || !watermark || watermark.conversationId !== selected || stopPending.current
            || !Number.isSafeInteger(watermark.inputRevision) || !Number.isSafeInteger(watermark.controlRevision)) return;
        const selection = { session: watermark.sessionId, conversation: selected,
            inputRevision: watermark.inputRevision!, controlRevision: watermark.controlRevision };
        if ((subagents.tasks?.unfinished ?? 0) > 0 || subagents.active_tasks.length > 0) {
            setStopConfirmation(selection);
        } else {
            void submitStop(selection, null);
        }
    }, [subagents, submitStop]);

    const confirmStop = useCallback((includeSubagents: boolean) => stopConfirmation
        ? submitStop(stopConfirmation, includeSubagents ? 'include_sub_agents' : 'main_only')
        : Promise.resolve(false), [stopConfirmation, submitStop]);

    const reset = useCallback((temporary = false) => {
        if (rehearsal) return;
        temporaryConversationRef.current = temporary;
        setTemporaryConversation(temporary);
        pendingDelivery.current = null;
        deliveryFailure.current = null;
        setDeliveryState(null);
        setAcceptedInput(null);
        activeRequest.current = null;
        snapshotActiveRequest.current = null;
        clearStopping(true);
        contextRequest.current = null;
        if (contextTimer.current !== null) window.clearTimeout(contextTimer.current);
        contextTimer.current = null;
        snapshotEpoch.current += 1;
        snapshotWatermark.current = null;
        queuedSession.current = null;
        queueStreamIds.current.clear();
        setRequestQueue(null);
        historyWindow.current = null;
        olderRequest.current = null;
        conversationId.current = null;
        currentTurnId.current = null;
        lastSeq.current = -1;
        previewArgs.current.clear();
        setMessages([]);
        setTools([]);
        setDraft(null);
        setPartial('');
        setStatus('idle');
        setError(null);
        setHydrating(false);
        setHistoryRestoreFailed(false);
        setAttachments([]);
        setFileScope({ revision: 0, directories: [] });
        directorySelectors.current = null;
        setVisualEvidence([]);
        setDocumentPreviews([]);
        setContextUsage(null);
        setContextNotices([]);
        setRemoteActive(false);
        setContextUpdating(false);
        setTaskStatusProjection(null);
        setGoal(null);
        setPendingGoalOpenRequest(null);
        setApprovalDelegation(null);
        setApprovalModelReadiness(null);
        setApprovalInitializing(false);
        setApprovalInitializationFailed(false);
        approvalInitializationAttempt.current = null;
        approvalContextRequest.current = null;
        setPermissionRequests([]);
        setBackgroundTasks([]);
        setCommandTasks([]);
        setSubagents({ active_tasks: [], task: null, tasks: null, parent_session_id: null, attention_tasks: [], attention_count: 0 });
        setMainStopped(false);
        setStopConfirmation(null);
        setCapabilityGrants([]);
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
    }, [conversationStorageScope, deskId, sendMessage, rehearsal]);

    const forgetConversation = useCallback((id: string | null) => {
        if (!id || conversationId.current !== id) return false;
        reset();
        return true;
    }, [reset]);

    const selectConversation = useCallback((id: string) => {
        if (rehearsal) return false;
        // Navigation must never cancel a turn or race an in-flight decision.
        if (!id || contextUpdating || permissionUpdating
            || grantRevoking || hydrating) return false;
        reset();
        conversationId.current = id;
        try {
            localStorage.setItem(storageKey(conversationStorageScope), id);
        } catch { /* Continuation still works without local storage. */ }
        void loadSnapshot(id, true, true);
        void loadRequestQueue(id);
        return true;
    }, [remoteActive, contextUpdating, permissionUpdating, grantRevoking,
        hydrating, reset, conversationStorageScope, loadRequestQueue, loadSnapshot, rehearsal]);

    const refreshSnapshot = useCallback(() => {
        if (conversationId.current) void loadSnapshot(conversationId.current);
    }, [loadSnapshot]);

    const resumeQueue = useCallback(async () => {
        const view = snapshotWatermark.current;
        if (!view || conversationId.current !== view.conversationId || resumingQueue || stopPending.current) return;
        const epoch = snapshotEpoch.current;
        setResumingQueue(true);
        try {
            const response = await fetch(`${REQUEST_QUEUE_PATH}/resume`, {
                method: 'POST', credentials: 'include',
                headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
                body: JSON.stringify({ session: view.sessionId, expectedControlRevision: view.controlRevision }),
                signal: AbortSignal.timeout(SNAPSHOT_TIMEOUT_MS),
            });
            const body = response.ok ? await response.json() : null;
            if (conversationId.current !== view.conversationId || epoch !== snapshotEpoch.current) return;
            await loadSnapshot(view.conversationId);
            if (body?.success !== true && conversationId.current === view.conversationId && epoch === snapshotEpoch.current) {
                setError(agentErrorMessage(translate.current, body?.code, body?.message,
                    translate.current('pages.aiAssistant.resumeQueueFailed')));
            }
        } catch {
            if (conversationId.current === view.conversationId && epoch === snapshotEpoch.current) {
                setError(translate.current('pages.aiAssistant.resumeQueueFailed'));
            }
        } finally {
            setResumingQueue(false);
        }
    }, [loadSnapshot, resumingQueue]);

    return {
        temporaryConversation,
        conversationId: conversationId.current,
        sessionId: snapshotWatermark.current?.conversationId === conversationId.current ? snapshotWatermark.current.sessionId : undefined,
        inputRevision: snapshotWatermark.current?.conversationId === conversationId.current ? snapshotWatermark.current.inputRevision : undefined,
        controlRevision: snapshotWatermark.current?.conversationId === conversationId.current ? snapshotWatermark.current.controlRevision : undefined,
        selectConversation,
        forgetConversation,
        contextUsage,
        contextNotices,
        messages,
        tools,
        draft,
        partial,
        status,
        error: historyRestoreFailed ? 'history_restore_failed' : error,
        historyRestoreFailed,
        attachments,
        fileScope,
        updateDirectory,
        visualEvidence,
        documentPreviews,
        requestDocumentPreviewPage,
        hydrating,
        contextUpdating,
        approvalInitializing,
        approvalInitializationFailed,
        initializeApprovalConversation,
        taskStatusProjection,
        goal,
        goalUpdating,
        controlGoal,
        pendingGoalOpenRequest,
        goalOpenUpdating,
        decideGoalOpen,
        approvalDelegation,
        approvalModelReadiness,
        approvalUpdating,
        setAutomaticApproval,
        permissionRequests,
        backgroundTasks,
        commandTasks,
        subagents,
        mainStopped,
        refreshSnapshot,
        taskCancelling,
        cancelTask,
        capabilityGrants,
        permissionUpdating,
        grantRevoking,
        pendingInputCount: pendingInputCount
            + (requestQueue?.conversation === conversationId.current ? requestQueue.items.filter(item => item.state === 'queued').length : 0),
        queuedRequests: requestQueue?.conversation === conversationId.current ? requestQueue.items.filter(item => item.state === 'queued') : [],
        failedRequests: requestQueue?.conversation === conversationId.current
            ? requestQueue.items.filter(item => item.state === 'failed') : [],
        cancelQueuedRequest,
        resumeQueue,
        resumingQueue,
        hasMoreMessages: messagePage.hasMore,
        loadingOlderMessages,
        sessionTarget,
        sessionTargets,
        sessionTargetReady,
        sessionTargetResolving,
        stop,
        stopping,
        stopConfirmation,
        dismissStopConfirmation: () => { if (!stopPending.current) setStopConfirmation(null); },
        confirmStop,
        canStop: !hydrating && !!snapshotWatermark.current && (remoteActive || !!(activeRequest.current ?? snapshotActiveRequest.current)
            || (subagents.tasks?.unfinished ?? 0) > 0 || subagents.active_tasks.length > 0
            || commandTasks.some(task => task.state === 'running')
            || backgroundTasks.some(task => task.state === 'running')
            || (!mainStopped && !!goal && ['queued', 'running', 'waiting_approval', 'waiting_work', 'waiting_device', 'waiting_model'].includes(goal.state))),
        turnRunning: activeRequest.current !== null || remoteActive,
        running: activeRequest.current !== null || remoteActive || deliveryState === 'sending'
            || contextUpdating || approvalInitializing || permissionUpdating || outcomeDisposing,
        start,
        deliveryState,
        acceptedInput,
        retryDelivery,
        updateContext,
        attachWindow,
        detachAttachment,
        decidePermission,
        decidePermissionItems,
        browserConnectionId,
        runId: snapshotWatermark.current?.sessionId ?? null,
        revokeCapabilityGrant,
        loadOlderMessages,
        selectSessionTarget,
        reset,
    };
}
