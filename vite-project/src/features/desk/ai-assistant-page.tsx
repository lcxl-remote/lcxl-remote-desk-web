import { permissionToolLabel, permissionResourceLabel, permissionOperationLabel } from './assistant-permission-labels';
import { AssistantAttachments, AssistantResultAttachments } from './assistant-attachments';
import { requireRecoveryZip } from '@/lib/file-recovery-error';
import { Textarea } from '@/components/ui/textarea';
import { Disclosure } from '@/components/ui/disclosure';
import { AssistantObservationResult } from './assistant-observation-result';
import { AssistantToolCall, isHistoricalPermissionSkip } from './assistant-tool-call';
import { useFollowLatest } from '@/hooks/use-follow-latest';
import './assistant-responsive.css';
import { AssistantSchedules } from './assistant-schedules';
import { AssistantImages } from './assistant-images';
import { AssistantDocumentPreviews } from './assistant-document-preview';
import { AssistantReasoning } from './assistant-reasoning';
import { AssistantBackgroundTasks } from './assistant-background-tasks';
import { ScheduleProposalCards } from '@/features/schedules/proposal-card';
import { AssistantContextMeter } from './assistant-context-meter';
import { AssistantDirectoryApproval } from './assistant-directory-approval';
import { AssistantToolGroup } from './assistant-tool-group';
import { AssistantFileScope } from './assistant-file-scope';
import { AssistantConnectionIcon } from './assistant-connection-icon';
import { AssistantCommandResult } from './assistant-command-result';
import { exportDeviceFileRecovery } from '@/services/clients';
import { FileRecoverySettings } from '@/features/settings/file-recovery-settings';
import { AssistantContextNotices, noticeMessageId } from './assistant-context-notices';
import { AssistantPermissionRequest } from './assistant-permission-request';
import { AssistantPermissionRecords } from './assistant-permission-records';
import { AssistantHistory } from './assistant-history';
import { AssistantMoreMenu, type AssistantMoreSection } from './assistant-more-menu';
import { useIsMobile } from '@/hooks/use-mobile';
import { DropdownMenu, DropdownMenuCheckboxItem, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger } from '@/components/ui/dropdown-menu';
import { Sheet, SheetContent, SheetHeader, SheetTitle } from '@/components/ui/sheet';
import { capabilityDescriptionKey } from './assistant-capability-copy';
import { Fragment, type FormEvent, useEffect, useState } from 'react';
import { Link, useLocation, useNavigate, useParams, useSearchParams } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { AlertTriangle, ArrowDown, ArrowLeft, CalendarClock, Check, Copy, Eye, FolderKey, ListTodo, LoaderCircle, Monitor, Paperclip, Plus, Puzzle, RefreshCw, Send, Settings2, ShieldCheck, X } from 'lucide-react';

import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Skeleton } from '@/components/ui/skeleton';
import { MarkdownContent } from '@/components/markdown-content';
import { useListConnections } from '@/services/hooks/connectionController/useListConnections';
import { useGetModelProvider } from '@/services/hooks/modelProviderController/useGetModelProvider';
import { useGetBrowserExtensionPairing } from '@/services/hooks/browserExtensionController/useGetBrowserExtensionPairing';
import { useQueryServerInfo } from '@/services/hooks/systemController/useQueryServerInfo';
import { useRestrictedSession } from './restricted-session';
import { useDeskSignaling } from './use-desk-signaling';
import { isAiAssistantEnabled } from './ai-assistant-switch';
import {
    type ObservationEntry,
    type OwnerSelectableWindow,
    type SelectableApplication,
    type ObservationRoot,
    ownerSelectableWindows,
    useAiAssistantObservation,
} from './use-ai-assistant-observation';
import { useAiAssistantChat, type AiAssistantMessage, type RehearsalConversation } from './use-ai-assistant-chat';
import { fetchGoalBudgetPolicy, type GoalBudgetPolicy } from '../settings/goal-budget-policy-settings';
import { AiAssistantRehearsalGate } from './ai-assistant-rehearsal-gate';
import { SessionTargetDialog } from './session-target-selection';
import { useAiAssistantCapabilities } from './use-ai-assistant-capabilities';
import { AssistantCapabilityList } from './assistant-capability-list';
import { AssistantDetailsSheet, type AssistantPanelId } from './assistant-details-sheet';
import { requiresBrowserRemoteTakeover } from './ai-assistant-browser-takeover';
import { useConfirmExec } from '../exec/use-confirm-exec';
import { ExecLifecycle } from '../exec/exec-lifecycle';
import {
    type AiAssistantFeatureProfile,
    hasAiAssistantBrowserEntry,
} from './ai-assistant-features';
import {
    isExactExternalSendTool,
    parseExternalSendReceipt,
} from './ai-assistant-external-send';

const CURRENT_SCREEN_CAPABILITY_ID = 'screen.capture.current';

function GoalLimitsSummary({
    policy,
}: {
    policy: GoalBudgetPolicy;
}) {
    const { t } = useTranslation();
    const items = [
        ['goalBudgetActiveHours', policy.limits.activeTimeMs, 3_600_000],
        ['goalBudgetDeadlineDays', policy.limits.deadlineMs, 86_400_000],
        ['goalBudgetTokens', policy.limits.modelTokens, 1],
        ['goalBudgetModelCalls', policy.limits.modelCalls, 1],
        ['goalBudgetToolCalls', policy.limits.toolCalls, 1],
        ['goalBudgetSlices', policy.limits.slices, 1],
        ['goalBudgetStalledSlices', policy.limits.stalledSlices, 1],
    ] as const;
    return <p className="mt-1 text-xs text-muted-foreground">{items.map(([label, value, scale]) =>
        `${t(`pages.aiAssistant.${label}`)}: ${value === null ? t('pages.aiAssistant.goalBudgetDisabled') : (value / scale).toLocaleString()}`,
    ).join(' · ')}</p>;
}

function ObservationCard({
    title,
    description,
    entry,
    onRefresh,
    disabled = false,
    windowCandidates = [],
    onAttachWindow,
    onDelayedRefresh,
    onCancelDelayed,
    remainingSeconds = 0,
    applications = [],
    onListApplications,
    onInspectApplication,
}: {
    title: string;
    description: string;
    entry: ObservationEntry;
    onRefresh: () => void;
    disabled?: boolean;
    windowCandidates?: OwnerSelectableWindow[];
    onAttachWindow?: (candidate: OwnerSelectableWindow) => void;
    onDelayedRefresh?: () => void;
    onCancelDelayed?: () => void;
    remainingSeconds?: number;
    applications?: SelectableApplication[];
    onListApplications?: () => void;
    onInspectApplication?: (root: ObservationRoot) => void;
}) {
    const { t } = useTranslation();
    const isPending = entry.phase === 'pending';
    const isScheduled = entry.phase === 'scheduled';
    const error = entry.outcome?.status === 'err' ? entry.outcome.data : null;

    return (
        <Card>
            <CardHeader>
                <div className="flex items-start justify-between gap-4">
                    <div>
                        <CardTitle className="flex items-center gap-2 text-base">
                            <Eye className="h-4 w-4" />
                            {title}
                        </CardTitle>
                        <CardDescription className="mt-1">{description}</CardDescription>
                    </div>
                    <Badge variant={entry.phase === 'ready' ? 'default' : 'outline'}>
                        {t(`pages.aiAssistant.phase.${entry.phase}`)}
                    </Badge>
                </div>
            </CardHeader>
            <CardContent className="space-y-3">
                <Button variant="outline" size="sm" onClick={onRefresh} disabled={disabled || isPending || isScheduled}>
                    {isPending
                        ? <LoaderCircle className="mr-2 h-4 w-4 animate-spin" />
                        : <RefreshCw className="mr-2 h-4 w-4" />}
                    {t('pages.aiAssistant.refresh')}
                </Button>
                {onDelayedRefresh && !isScheduled && (
                    <Button className="ml-2" variant="outline" size="sm" onClick={onDelayedRefresh} disabled={disabled || isPending}>
                        {t('pages.aiAssistant.delayedObserve')}
                    </Button>
                )}
                {isScheduled && (
                    <div role="status" className="space-y-2 text-sm">
                        <p>{t('pages.aiAssistant.observationCountdown', { count: remainingSeconds })}</p>
                        <Button variant="outline" size="sm" onClick={onCancelDelayed}>
                            {t('pages.aiAssistant.cancelObservation')}
                        </Button>
                    </div>
                )}
                {onListApplications && (
                    <div className="space-y-2 rounded-md border p-3">
                        <Button variant="outline" size="sm" onClick={onListApplications} disabled={disabled || isPending || isScheduled}>
                            {t('pages.aiAssistant.listApplications')}
                        </Button>
                        <p className="text-xs text-muted-foreground">{t('pages.aiAssistant.applicationSelectionHint')}</p>
                        <div className="flex flex-wrap gap-2">
                            {applications.map((app) => <Button key={app.objectRef.token} variant="outline" size="sm"
                                disabled={disabled || isPending || isScheduled || Date.parse(app.objectRef.expires_at) <= Date.now()}
                                onClick={() => onInspectApplication?.(app.objectRef)}>
                                {app.name || t('pages.aiAssistant.applicationUnnamed')}
                            </Button>)}
                        </div>
                    </div>
                )}
                {error && (
                    <Alert variant="destructive">
                        <AlertTitle>{error.kind}</AlertTitle>
                        <AlertDescription>{error.message}</AlertDescription>
                        {error.kind === 'permission_denied' && error.message.includes('allowlist') && (
                            <AlertDescription>{t('pages.aiAssistant.applicationRestrictionHint')}</AlertDescription>
                        )}
                    </Alert>
                )}
                {windowCandidates.length > 0 && onAttachWindow && (
                    <div className="space-y-2 rounded-md border p-3">
                        <div>
                            <p className="text-sm font-medium">
                                {t('pages.aiAssistant.windowSelectorTitle')}
                            </p>
                            <p className="text-xs text-muted-foreground">
                                {t('pages.aiAssistant.windowSelectorDescription')}
                            </p>
                        </div>
                        <div className="flex flex-wrap gap-2">
                            {windowCandidates.map((candidate) => {
                                const label = candidate.title
                                    ?? t('pages.aiAssistant.windowSelectorUntitled');
                                return (
                                    <Button
                                        key={candidate.objectRef.token}
                                        type="button"
                                        size="sm"
                                        variant="secondary"
                                        disabled={disabled}
                                        onClick={() => onAttachWindow(candidate)}
                                    >
                                        {label}
                                    </Button>
                                );
                            })}
                        </div>
                    </div>
                )}
                {entry.outcome?.status === 'ok' && (
                    <AssistantObservationResult data={entry.outcome.data} />
                )}
            </CardContent>
        </Card>
    );
}

export function AiAssistantWorkspace({
    rehearsal,
    initialConversationId,
    deskId,
    stableDeviceId,
    localPairingAvailable,
    featureProfile,
    assistantEnabled,
    backTo,
}: {
    rehearsal?: RehearsalConversation;
    initialConversationId?: string | null;
    deskId: string;
    stableDeviceId: string;
    localPairingAvailable: boolean;
    featureProfile: AiAssistantFeatureProfile;
    assistantEnabled: boolean;
    backTo?: string;
}) {
    const { t } = useTranslation();
    const { i18n } = useTranslation();
    const isMobile = useIsMobile();
    const { isConnected, subscribe, sendMessage } = useDeskSignaling();
    const { entries, inspectSession, inspectUi, scheduleUi, cancelDelayedUi, remainingSeconds, applications, listApplications, applicationSelectionAvailable } = useAiAssistantObservation({
        deskId,
        enabled: assistantEnabled && isConnected,
        subscribe,
        sendMessage,
    });
    const scheduleNavigate = useNavigate();
    const setupOrigin = useLocation();
    const chat = useAiAssistantChat({
        rehearsal,
        initialConversationId,
        deskId,
        connected: isConnected,
        conversationStorageScope: stableDeviceId,
        subscribe,
        sendMessage,
    });
    const capabilities = useAiAssistantCapabilities({
        deskId, subscribe, sendMessage, enabled: assistantEnabled && isConnected,
    });
    const recoveryConnections = useListConnections();
    const exportBackup = async (id: string) => {
        // Recovery records use the durable server session key, not the client
        // conversation UUID used when submitting new assistant turns.
        const conversation = chat.sessionId;
        const connection = recoveryConnections.data?.find(item => item.connection_id === deskId);
        if (!conversation || !connection) throw new Error('Backup target unavailable');
        const data = await exportDeviceFileRecovery({ connection: deskId, device_id: connection.device_id,
            conversation_id: conversation, recovery_id: id }, { responseType: 'blob' });
        const zip = await requireRecoveryZip(data);
        const url = URL.createObjectURL(zip);
        const anchor = document.createElement('a');
        anchor.href = url; anchor.download = 'file-recovery.zip'; anchor.click();
        window.setTimeout(() => URL.revokeObjectURL(url), 60000);
    };
    const exec = useConfirmExec({
        deskId,
        deviceId: stableDeviceId,
        subscribe,
        sendMessage,
        acceptUnsolicitedPreviews: true,
    });
    const provider = useGetModelProvider();
    const browserPairing = useGetBrowserExtensionPairing({
        query: { enabled: false, retry: false },
    });
    const providerConfig = provider.data?.data;
    const pairing = browserPairing.data?.data;
    const [pairingCopied, setPairingCopied] = useState(false);
    const [question, setQuestion] = useState(rehearsal?.status === 'pending' ? rehearsal.prompt : '');
    const rehearsalCanStart = !rehearsal || (rehearsal.status === 'pending' && !chat.running && !chat.messages.some(message => message.role === 'user'));
    const [schedulesOpen, setSchedulesOpen] = useState(false);
    const [taskPanelSession, setTaskPanelSession] = useState<string | null>(null);
    const [attachmentsOpen, setAttachmentsOpen] = useState(false);
    const [goalDetailsOpen, setGoalDetailsOpen] = useState(false);
    const [approvalSettingsOpen, setApprovalSettingsOpen] = useState(false);
    const [addContextOpen, setAddContextOpen] = useState(false);
    const [pendingScheduleCount, setPendingScheduleCount] = useState(0);
    const [deviceSettingsOpen, setDeviceSettingsOpen] = useState(false);
    const [panel, setPanel] = useState<AssistantPanelId | null>(null);
    const [permissionHistorySession, setPermissionHistorySession] = useState<string | null>(null);
    const [directorySession, setDirectorySession] = useState<string | null>(null);
    const permissionHistoryKey = `${deskId}:${chat.conversationId}`;
    const { scrollRef, contentRef, onScroll, showJumpToLatest, jumpToLatest } = useFollowLatest(true, permissionHistoryKey);
    const pendingDirectories = chat.fileScope.directories.filter(directory => directory.state === 'pending');
    const pendingPermissionCount = chat.permissionRequests.filter(request => request.state === 'pending').length;
    const pendingCount = pendingDirectories.length + pendingPermissionCount + pendingScheduleCount + Number(Boolean(chat.pendingGoalOpenRequest));
    const runningTaskCount = [...chat.commandTasks, ...chat.backgroundTasks]
        .filter(task => ['running', 'cancel_requested'].includes(task.state)).length;
    const jumpToPending = (id?: string) => {
        const target = id ? document.getElementById(id) : document.querySelector<HTMLElement>('[data-assistant-pending]');
        target?.scrollIntoView({ block: 'center', behavior: 'smooth' });
        target?.focus({ preventScroll: true });
    };
    useEffect(() => { setPermissionHistorySession(null); }, [permissionHistoryKey]);
    useEffect(() => { setPendingScheduleCount(0); }, [permissionHistoryKey]);
    const [selectedCapabilityIds, setSelectedCapabilityIds] = useState<string[]>([]);
    const selectedContextCount = new Set([...selectedCapabilityIds,
        ...chat.attachments.filter(item => item.state === 'active').map(item => item.capabilityId)]).size;
    const [startGoal, setStartGoal] = useState(false);
    const [goalBudgetPolicy, setGoalBudgetPolicy] = useState<GoalBudgetPolicy | null>(null);
    useEffect(() => {
        if (!startGoal && !chat.pendingGoalOpenRequest && !chat.goal) return;
        let current = true;
        setGoalBudgetPolicy(null);
        const reload = () => { void fetchGoalBudgetPolicy()
            .then(value => { if (current) setGoalBudgetPolicy(value); })
            .catch(() => { if (current) setGoalBudgetPolicy(null); }); };
        reload();
        const timer = window.setInterval(reload, 15_000);
        return () => { current = false; window.clearInterval(timer); };
    }, [startGoal, chat.pendingGoalOpenRequest?.requestId, chat.goal?.goalId]);
    const [previousCompletedGoalId, setPreviousCompletedGoalId] = useState<string | null>(null);
    const [offPageReminderAvailable, setOffPageReminderAvailable] = useState(() => {
        try { return sessionStorage.getItem('lcxl.tauriShell') === '1'; }
        catch { return false; }
    });
    useEffect(() => {
        const abort = new AbortController();
        void fetch('/api/my/ai-assistant-attention?limit=1', {
            credentials: 'include', headers: { Accept: 'application/json' }, signal: abort.signal,
        }).then(response => response.ok ? response.json() : null)
            .then(body => {
                if (!abort.signal.aborted && body?.data?.offPageReminderAvailable === true) {
                    setOffPageReminderAvailable(true);
                }
            }).catch(() => {});
        return () => abort.abort();
    }, []);

    const contextCapabilities = featureProfile.object_context
        ? (capabilities.snapshot?.entries ?? []).filter((entry) => entry.context_selectable)
        : [];

    const browserTakeoverRequired = requiresBrowserRemoteTakeover(
        capabilities.snapshot?.entries,
    );
    const externalSendReceipts = chat.tools.flatMap((tool) => {
        if (!isExactExternalSendTool(tool.name)) return [];
        const receipt = parseExternalSendReceipt(tool.output);
        return receipt ? [{ tool, receipt }] : [];
    });

    useEffect(() => {
        const ready = new Set(
            contextCapabilities
                .filter((entry) => entry.ready)
                .map((entry) => entry.capability.capability_id),
        );
        setSelectedCapabilityIds((current) => current.filter((id) => ready.has(id)));
    }, [capabilities.snapshot]);

    useEffect(() => {
        const restored = featureProfile.object_context
            ? chat.attachments
            .filter((attachment) =>
                attachment.state === 'active' && attachment.kind === 'interactive_session',
            )
            .map((attachment) => attachment.capabilityId)
            : [];
        setSelectedCapabilityIds([...new Set(restored)]);
    }, [chat.attachments, featureProfile.object_context]);

    const toggleContext = (capabilityId: string) => {
        if (!assistantEnabled || !featureProfile.object_context) return;
        const next = selectedCapabilityIds.includes(capabilityId)
            ? selectedCapabilityIds.filter((id) => id !== capabilityId)
            : [...selectedCapabilityIds, capabilityId];
        // CurrentScreen is deliberately one-shot: selecting it never writes a
        // durable context update, and the UI clears it immediately after the
        // turn is accepted so every screenshot requires a fresh user gesture.
        if (capabilityId === CURRENT_SCREEN_CAPABILITY_ID) {
            setSelectedCapabilityIds(next);
            return;
        }
        if (chat.updateContext(next)) setSelectedCapabilityIds(next);
    };

    useEffect(() => {
        const accepted = chat.acceptedInput;
        if (!accepted) return;
        setQuestion(current => current.trim() === accepted.question ? '' : current);
        setStartGoal(false);
        setPreviousCompletedGoalId(null);
        setSelectedCapabilityIds(current => current.filter(id => id !== CURRENT_SCREEN_CAPABILITY_ID));
    }, [chat.acceptedInput]);

    const submit = (event: FormEvent) => {
        event.preventDefault();
        if (chat.turnRunning || chat.deliveryState) return;
        if (!assistantEnabled || !rehearsalCanStart || (startGoal && !goalBudgetPolicy)) return;
        const selectedContext = featureProfile.object_context ? selectedCapabilityIds : [];
        chat.start(question, i18n.language, selectedContext, startGoal, previousCompletedGoalId);
    };

    const resetConversation = () => {
        chat.reset();
        setSelectedCapabilityIds([]);
        setStartGoal(false);
        setPreviousCompletedGoalId(null);
    };

    const displayNameForTool = (name: string) => {
        const key = capabilities.snapshot?.entries.find(entry => entry.capability.tool_name === name)?.capability.display_name_key;
        return key ? t(key, { defaultValue: name }) : name;
    };
    const displayNameForCall = (callId?: string) => {
        const name = chat.tools.find(tool => tool.callId === callId)?.name;
        return name ? displayNameForTool(name) : undefined;
    };

    const renderTranscriptMessage = (message: AiAssistantMessage) => (
                            <Fragment key={message.id}>
                            {(message.role !== 'assistant' || message.text || message.reasoning) && <div
                                key={message.id}
                                id={message.role === 'tool_call' ? `assistant-call-${message.toolCallId}` : undefined}
                                tabIndex={message.role === 'tool_call' ? -1 : undefined}
                                className={`max-w-[90%] rounded-lg px-3 py-2 text-sm ${
                                    message.role === 'user'
                                        ? 'ml-auto bg-muted'
                                        : message.role === 'tool_result' ? 'w-full border bg-muted/30' : 'w-full bg-transparent'
                                }`}
                            >
                                {message.role === 'tool_call' ? <AssistantToolCall tool={chat.tools.find(tool => tool.callId === message.toolCallId)} running={chat.running}
                                    displayName={displayNameForCall(message.toolCallId)} /> : message.role === 'tool_result' ? <>
                                    {message.permissionReason && <p className="mb-2 text-sm">{t('pages.aiAssistant.permissionReasonLabel', { reason: message.permissionReason })}</p>}
                                    {isHistoricalPermissionSkip(message.text) && <p className="mb-2 text-sm text-amber-700 dark:text-amber-300">{t('pages.aiAssistant.historicalPermissionSkip')}</p>}
                                    <AssistantCommandResult text={message.text} tool={chat.tools.find(tool => tool.callId === message.toolCallId)} onLocateCall={message.toolCallId ? () => { const target = document.getElementById(`assistant-call-${message.toolCallId}`); target?.scrollIntoView({ block: 'center', behavior: 'smooth' }); target?.focus({ preventScroll: true }); } : undefined} onExportBackup={exportBackup} />
                                    <AssistantResultAttachments sessionId={chat.sessionId} text={message.text} />
                                </> : message.role === 'assistant'
                                    ? <><AssistantReasoning text={message.reasoning} />{message.text && <MarkdownContent disableLinks>{message.text}</MarkdownContent>}</>
                                    : <p className="whitespace-pre-wrap">{message.text}</p>}
                            </div>}
                            <AssistantContextNotices notices={chat.contextNotices.filter(notice => noticeMessageId(notice, chat.messages) === message.id)} />
                            </Fragment>
                        );

    const detailsContent = (
        <div className="space-y-4">
            <p className="text-sm text-muted-foreground">{t('pages.aiAssistant.providerBoundary', {
                provider: providerConfig?.wire_protocol ?? t('pages.aiAssistant.providerUnknown'),
                model: providerConfig?.model ?? t('pages.aiAssistant.providerUnknown'),
            })}</p>
                                {chat.taskStatusProjection && (
                        <div data-testid="ai-assistant-task-status" className="space-y-2 rounded-md border p-3">
                            <div className="flex flex-wrap items-center justify-between gap-2">
                                <div>
                                    <p className="text-sm font-medium">{t('pages.aiAssistant.taskStatusTitle')}</p>
                                    <p className="text-xs text-muted-foreground">
                                        {t('pages.aiAssistant.taskStatusDescription')}
                                    </p>
                                </div>
                                <div className="flex gap-2">
                                    {chat.pendingInputCount > 0 && (
                                        <Badge variant="secondary">
                                            {t('pages.aiAssistant.pendingInputs', { count: chat.pendingInputCount })}
                                        </Badge>
                                    )}
                                    <Badge variant="outline">rev {chat.taskStatusProjection.revision}</Badge>
                                </div>
                            </div>
                            <div className="space-y-2">
                                {chat.taskStatusProjection.items.map((item) => (
                                    <div key={item.itemId} className="flex items-start justify-between gap-3 rounded bg-muted/50 px-3 py-2">
                                        <div>
                                            <p className="text-sm">{item.description}</p>
                                            {item.note && <p className="text-xs text-muted-foreground">{item.note}</p>}
                                        </div>
                                        <Badge variant="outline">
                                            {t(`pages.aiAssistant.taskStatus.${item.status}`)}
                                        </Badge>
                                    </div>
                                ))}
                            </div>
                        </div>
                    )}

                                {chat.capabilityGrants.length > 0 && (
                        <div data-testid="ai-assistant-capability-grants" className="space-y-3 rounded-md border border-emerald-500/40 p-3">
                            <div>
                                <p className="flex items-center gap-2 text-sm font-medium">
                                    <ShieldCheck className="h-4 w-4" />
                                    {t('pages.aiAssistant.grantTitle')}
                                </p>
                                <p className="text-xs text-muted-foreground">
                                    {t('pages.aiAssistant.grantDescription')}
                                </p>
                            </div>
                            {chat.capabilityGrants.map((grant) => {
                                const expired = grant.expiresAtUnixMs <= Date.now();
                                const state = grant.revokedAtUnixMs != null
                                    ? 'revoked'
                                    : expired
                                        ? 'expired'
                                        : grant.remainingUses === 0
                                            ? 'exhausted'
                                            : 'active';
                                return (
                                    <div key={grant.grantId} className="space-y-2 rounded-md bg-muted/50 p-3">
                                        <div className="flex flex-wrap items-start justify-between gap-2">
                                            <div>
                                                <p className="break-all text-sm font-medium">
                                                    {permissionToolLabel(t, grant.toolName)}
                                                </p>
                                                <p className="break-all text-xs text-muted-foreground">
                                                    {grant.providerId} · {grant.capabilityId} · {grant.riskTier}
                                                </p>
                                            </div>
                                            <Badge variant={state === 'active' ? 'default' : 'outline'}>
                                                {t(`pages.aiAssistant.grantState.${state}`)}
                                            </Badge>
                                        </div>
                                        <div className="space-y-1 text-xs text-muted-foreground">
                                            <p>{t('pages.aiAssistant.grantRemainingUses', { count: grant.remainingUses })}</p>
                                            <p>{t('pages.aiAssistant.grantExpiresAt', {
                                                time: new Date(grant.expiresAtUnixMs).toLocaleString(),
                                            })}</p>
                                            {[...grant.resourceScope, ...grant.operationScope].length > 0 && (
                                                <p className="break-all">
                                                    {[...grant.resourceScope.map((scope) => permissionResourceLabel(t, scope)), ...grant.operationScope.map((scope) => permissionOperationLabel(t, scope))].join(' · ')}
                                                </p>
                                            )}
                                            {grant.revokedReason && <p className="break-all">{grant.revokedReason}</p>}
                                        </div>
                                        {featureProfile.grant_revoke && state === 'active' && (
                                            <Button
                                                type="button"
                                                size="sm"
                                                variant="outline"
                                                disabled={chat.grantRevoking !== null}
                                                onClick={() => void chat.revokeCapabilityGrant(grant.grantId)}
                                            >
                                                {chat.grantRevoking === grant.grantId
                                                    ? <LoaderCircle className="mr-2 h-4 w-4 animate-spin" />
                                                    : <X className="mr-2 h-4 w-4" />}
                                                {t('pages.aiAssistant.grantRevoke')}
                                            </Button>
                                        )}
                                    </div>
                                );
                            })}
                        </div>
                    )}

            {chat.tools.length === 0 && <p className="text-sm text-muted-foreground">{t('pages.aiAssistant.workspace.emptyActivity')}</p>}
            {chat.tools.map((tool) => (
                <Disclosure key={tool.callId} className="rounded-lg border p-3" title={<>{tool.name} · {t(`pages.aiAssistant.workspace.toolState.${tool.status}`)}</>} summaryClassName="cursor-pointer text-sm">

                    <p className="mt-2 text-sm">{tool.permissionReason && t('pages.aiAssistant.permissionReasonLabel', { reason: tool.permissionReason })}</p>
                    <pre className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap break-words text-xs">{tool.argumentsJson}</pre>
                    {tool.output && <pre className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap break-words text-xs">{tool.output}</pre>}
                </Disclosure>
            ))}
                                {chat.draft && (
                        <Card data-testid="computer-action-draft-preview" className="border-violet-500/40">
                            <CardHeader>
                                <CardTitle className="text-base">{t('pages.aiAssistant.draftTitle')}</CardTitle>
                                <CardDescription>
                                    {t('pages.aiAssistant.draftDescription', {
                                        count: chat.draft.actions.length,
                                        risk: chat.draft.risk,
                                    })}
                                </CardDescription>
                            </CardHeader>
                            <CardContent className="space-y-3">
                                <pre className="max-h-64 overflow-auto whitespace-pre-wrap break-words rounded-md bg-muted p-3 text-xs">
                                    {JSON.stringify(chat.draft, null, 2)}
                                </pre>
                                <Button disabled>{t('pages.aiAssistant.executionDisabled')}</Button>
                            </CardContent>
                        </Card>
                    )}

        </div>
    );

    const moreSections: AssistantMoreSection[] = [
        { label: t('pages.aiAssistant.workspace.resources'), actions: [
            { label: t('pages.aiAssistant.attachments.title'), icon: Paperclip,
                disabled: !chat.sessionId, onSelect: () => setAttachmentsOpen(true) },
            { label: t('pages.aiAssistant.directories.title'), icon: FolderKey,
                onSelect: () => setDirectorySession(permissionHistoryKey) },
            { label: t('pages.aiAssistant.schedules.title'), icon: CalendarClock,
                onSelect: () => setSchedulesOpen(true) },
            ...(!rehearsal ? [{ label: t('schedules.createResume'), icon: CalendarClock,
                disabled: !assistantEnabled || chat.running || chat.hydrating || !chat.conversationId || !chat.inputRevision,
                onSelect: () => { if (chat.conversationId && chat.inputRevision) scheduleNavigate(`/schedules?${new URLSearchParams({
                    resume_conversation: chat.conversationId, resume_device: stableDeviceId,
                    resume_revision: String(chat.inputRevision),
                })}`); } }] : []),
        ] },
        { label: t('pages.aiAssistant.workspace.manage'), actions: [
            { label: `${t('pages.aiAssistant.tasks.title')}${runningTaskCount > 0 ? ` (${runningTaskCount})` : ''}`,
                icon: ListTodo, onSelect: () => setTaskPanelSession(permissionHistoryKey) },
            { label: t('pages.aiAssistant.permissionHistory'), icon: ShieldCheck,
                onSelect: () => setPermissionHistorySession(permissionHistoryKey) },
            ...(featureProfile.approval_delegation ? [{ label: t('pages.aiAssistant.autoApprovalTitle'), icon: ShieldCheck,
                onSelect: () => setApprovalSettingsOpen(true) }] : []),
            { label: t('pages.aiAssistant.workspace.deviceSettings'), icon: Settings2,
                onSelect: () => setDeviceSettingsOpen(true) },
        ] },
        { label: t('pages.aiAssistant.workspace.troubleshoot'), actions: [
            { label: t('pages.aiAssistant.workspace.details'), onSelect: () => setPanel('details') },
        ] },
    ];

    return (
        <>
            <AssistantDetailsSheet panel={panel} onPanelChange={setPanel} sections={{
                details: detailsContent,
                capabilities: <AssistantCapabilityList entries={capabilities.snapshot?.entries ?? []}
                    loading={capabilities.loading} error={Boolean(capabilities.error)}
                    refreshDisabled={!assistantEnabled || !isConnected} onRefresh={capabilities.refresh} />,
                context: <>            {featureProfile.object_context && (
            <Card data-testid="ai-assistant-context-selector">
                <CardHeader>
                    <CardTitle className="text-base">{t('pages.aiAssistant.contextTitle')}</CardTitle>
                    <CardDescription>{t('pages.aiAssistant.contextDescription')}</CardDescription>
                </CardHeader>
                <CardContent className="space-y-2">
                    {contextCapabilities.length === 0 && (
                        <p className="text-sm text-muted-foreground">
                            {t('pages.aiAssistant.contextEmpty')}
                        </p>
                    )}
                    {contextCapabilities.map((entry) => {
                        const id = entry.capability.capability_id;
                        const selected = selectedCapabilityIds.includes(id);
                        return (
                            <Button variant="unstyled"
                                key={id}
                                type="button"
                                disabled={!assistantEnabled || !entry.ready || chat.running}
                                onClick={() => toggleContext(id)}
                                className="flex w-full items-center justify-between gap-3 rounded-md border px-3 py-2 text-left disabled:cursor-not-allowed disabled:opacity-50"
                            >
                                <span>
                                    <span className="block text-sm font-medium">{t(entry.capability.display_name_key, { defaultValue: id })}</span>
                                    <code className="block break-all text-xs text-muted-foreground">{id}</code>
                                    <span className="block text-xs text-muted-foreground">
                                        {t(capabilityDescriptionKey(entry.capability.display_name_key), {
                                            defaultValue: t('pages.aiAssistant.workspace.descriptionUnavailable'),
                                        })}
                                    </span>
                                    <span className="block text-xs text-muted-foreground">
                                        {entry.ready
                                            ? t('pages.aiAssistant.contextWillSend')
                                            : entry.reason ?? t('pages.aiAssistant.contextUnavailable')}
                                    </span>
                                </span>
                                <Badge variant={selected ? 'default' : 'outline'}>
                                    {selected && <Check className="mr-1 h-3 w-3" />}
                                    {selected
                                        ? t('pages.aiAssistant.contextSelected')
                                        : t('pages.aiAssistant.contextNotSelected')}
                                </Badge>
                            </Button>
                        );
                    })}
                    {chat.attachments.length > 0 && (
                        <div className="space-y-2 border-t pt-3" data-testid="ai-assistant-attachments">
                            <p className="text-xs font-medium text-muted-foreground">
                                {t('pages.aiAssistant.attachmentTitle')}
                            </p>
                            {chat.attachments.map((attachment) => (
                                <div
                                    key={attachment.id}
                                    className="flex items-center justify-between gap-3 rounded-md bg-muted px-3 py-2"
                                >
                                    <span className="min-w-0">
                                        <span className="block truncate text-xs font-medium">
                                            {attachment.displaySummary}
                                        </span>
                                        <span className="block text-xs text-muted-foreground">
                                            {attachment.providerId} · {attachment.kind}
                                        </span>
                                    </span>
                                    <span className="flex shrink-0 items-center gap-1">
                                        <Badge variant={attachment.state === 'active' ? 'secondary' : 'outline'}>
                                            {attachment.state === 'active'
                                                ? t('pages.aiAssistant.attachmentActive')
                                                : t('pages.aiAssistant.attachmentStale', {
                                                    reason: attachment.staleReason ?? 'unknown',
                                                })}
                                        </Badge>
                                        {attachment.state === 'active' &&
                                            attachment.kind !== 'interactive_session' && (
                                            <Button
                                                type="button"
                                                variant="ghost"
                                                size="icon"
                                                className="h-7 w-7"
                                                disabled={chat.running}
                                                title={t('pages.aiAssistant.attachmentDetach')}
                                                onClick={() => chat.detachAttachment(attachment.id)}
                                            >
                                                <X className="h-3.5 w-3.5" />
                                            </Button>
                                        )}
                                    </span>
                                </div>
                            ))}
                        </div>
                    )}
                </CardContent>
            </Card>
            )}
</>,
                connection: <>
            {recoveryConnections.data?.some(item => item.connection_id === deskId) && <FileRecoverySettings
                key={deskId} target={{ connection: deskId, device_id: recoveryConnections.data.find(item => item.connection_id === deskId)?.device_id }} />}
            {localPairingAvailable && (
                <Card data-testid="browser-extension-pairing">
                    <CardHeader>
                        <CardTitle className="flex items-center gap-2 text-base">
                            <Puzzle className="h-4 w-4" />
                            {t('pages.aiAssistant.browserExtensionTitle')}
                        </CardTitle>
                        <CardDescription>
                            {t('pages.aiAssistant.browserExtensionDescription')}
                        </CardDescription>
                    </CardHeader>
                    <CardContent className="space-y-3">
                        {!pairing && (
                            <Button
                                variant="outline"
                                onClick={() => browserPairing.refetch()}
                                disabled={!assistantEnabled || browserPairing.isFetching}
                            >
                                {browserPairing.isFetching && (
                                    <LoaderCircle className="mr-2 h-4 w-4 animate-spin" />
                                )}
                                {t('pages.aiAssistant.browserExtensionShowCode')}
                            </Button>
                        )}
                        {browserPairing.isError && (
                            <Alert variant="destructive">
                                <AlertDescription>
                                    {t('pages.aiAssistant.browserExtensionUnavailable')}
                                </AlertDescription>
                            </Alert>
                        )}
                        {pairing && (
                            <div className="space-y-2">
                                <div className="flex gap-2">
                                    <Input
                                        aria-label={t('pages.aiAssistant.browserExtensionPairingCode')}
                                        readOnly
                                        value={pairing.pairing_code}
                                        className="font-mono text-xs"
                                    />
                                    <Button
                                        variant="outline"
                                        size="icon"
                                        aria-label={t('pages.aiAssistant.browserExtensionCopyCode')}
                                        onClick={async () => {
                                            await navigator.clipboard.writeText(pairing.pairing_code);
                                            setPairingCopied(true);
                                        }}
                                    >
                                        {pairingCopied
                                            ? <Check className="h-4 w-4" />
                                            : <Copy className="h-4 w-4" />}
                                    </Button>
                                </div>
                                <p className="break-all text-xs text-muted-foreground">
                                    {t('pages.aiAssistant.browserExtensionBridge', {
                                        bridge: pairing.bridge_url,
                                        version: pairing.extension_version,
                                    })}
                                </p>
                            </div>
                        )}
                    </CardContent>
                </Card>
            )}
</>,
                observation: <>            <div className="grid gap-4 lg:grid-cols-2">
                <ObservationCard
                    title={t('pages.aiAssistant.sessionTitle')}
                    description={t('pages.aiAssistant.sessionDescription')}
                    entry={entries.desktop_session_inspect}
                    onRefresh={() => inspectSession()}
                    disabled={!assistantEnabled || !isConnected}
                />
                <ObservationCard
                    title={t('pages.aiAssistant.uiTitle')}
                    description={t('pages.aiAssistant.uiDescription')}
                    entry={entries.desktop_ui_inspect}
                    onRefresh={() => inspectUi()}
                    applications={applications}
                    onListApplications={applicationSelectionAvailable ? listApplications : undefined}
                    onInspectApplication={inspectUi}
                    onDelayedRefresh={scheduleUi}
                    onCancelDelayed={cancelDelayedUi}
                    remainingSeconds={remainingSeconds}
                    disabled={!assistantEnabled || !isConnected}
                    windowCandidates={ownerSelectableWindows(entries.desktop_ui_inspect)}
                    onAttachWindow={(candidate) => chat.attachWindow(
                        candidate.objectRef,
                        candidate.title ?? t('pages.aiAssistant.windowSelectorUntitled'),
                    )}
                />
            </div>
</>,
            }} />
            <SessionTargetDialog
                targets={chat.sessionTargets}
                onSelect={(targetId) => chat.selectSessionTarget(targetId)}
            />
            {!assistantEnabled && (
                <Alert data-testid="ai-assistant-disabled">
                    <AlertTitle>{t('pages.aiAssistant.disabledTitle')}</AlertTitle>
                    <AlertDescription>{t('pages.aiAssistant.disabledDescription')}
                        <Button type="button" variant="outline" size="sm" className="mt-2" onClick={() => setDeviceSettingsOpen(true)}>
                            {t('pages.aiAssistant.workspace.deviceSettings')}
                        </Button>
                    </AlertDescription>
                </Alert>
            )}
            {[
                featureProfile.permission_decision,
                featureProfile.approval_delegation,
                featureProfile.grant_revoke,
                featureProfile.background_task_cancel,
                featureProfile.object_context,
            ].some((enabled) => !enabled) && (
                <Alert data-testid="ai-assistant-partial-support">
                    <AlertTitle>{t('pages.aiAssistant.partialSupportTitle')}</AlertTitle>
                    <AlertDescription>
                        {t('pages.aiAssistant.partialSupportDescription')}
                    </AlertDescription>
                </Alert>
            )}
            {browserTakeoverRequired && (
                <Link to={`/desk/${encodeURIComponent(deskId)}/browser-setup`}
                    state={{ returnTo: setupOrigin.pathname + setupOrigin.search }} data-testid="browser-remote-takeover"
                    className="flex w-full shrink-0 items-center gap-2 border bg-muted/40 px-3 py-2 text-sm hover:bg-muted focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring">
                    <Monitor className="h-4 w-4 shrink-0 text-muted-foreground" aria-hidden="true" />
                    <span className="min-w-0 flex-1">{t('pages.aiAssistant.browserTakeoverTitle')}</span>
                    <span className="shrink-0 text-xs text-primary">{t('pages.aiAssistant.browserSetup.open')}</span>
                </Link>
            )}
            <Card className="flex min-h-0 w-full flex-1 flex-col rounded-none border-0 shadow-none">
                <CardHeader className="assistant-header shrink-0 px-3 py-2">
                    <div data-testid="assistant-title-row" className="flex items-center justify-between gap-2">
                        <div className="flex min-w-0 flex-1 items-center gap-2">
                            {backTo && (
                                <Button asChild variant="ghost" size="icon" className="assistant-header-back h-8 w-8 shrink-0">
                                    <Link to={backTo} aria-label={t('pages.aiAssistant.backToDevice')} title={t('pages.aiAssistant.backToDevice')}>
                                        <ArrowLeft className="h-4 w-4" aria-hidden="true" />
                                    </Link>
                                </Button>
                            )}
                            <CardTitle className="flex min-w-0 items-center gap-2 text-base">
                                <AssistantConnectionIcon connected={isConnected} enabled={assistantEnabled} />
                                <span title={recoveryConnections.data?.find(item => item.connection_id === deskId)?.version_info.display_name ?? deskId}
                                    className="truncate">{recoveryConnections.data?.find(item => item.connection_id === deskId)?.version_info.display_name
                                        || chat.sessionTarget?.display_name || t('pages.aiAssistant.chatTitle')}</span>
                            </CardTitle>
                        </div>
                        <div className="assistant-header-actions flex shrink-0 items-center gap-1">
                            <AssistantHistory deskId={deskId} deviceId={recoveryConnections.data?.find(item => item.connection_id === deskId)?.device_id} disabled={!!rehearsal || chat.hydrating || chat.contextUpdating || chat.permissionUpdating || !!chat.grantRevoking}
                                onDeleted={id => { if (chat.forgetConversation(id)) setSelectedCapabilityIds([]); }}
                                onSelect={(id) => {
                                    if (!chat.selectConversation(id)) return false;
                                    setQuestion('');
                                    setSelectedCapabilityIds([]);
                                    return true;
                                }} onNew={resetConversation} />
                            <AssistantMoreMenu sections={moreSections} />
                        </div>
                    </div>
                </CardHeader>
                <CardContent className="flex min-h-0 flex-1 flex-col gap-2 px-3 pb-3 pt-0">
                    {(pendingCount > 0 || runningTaskCount > 0 || chat.goal || chat.approvalDelegation?.status === 'active') && (
                        <div className="flex w-full min-w-0 shrink-0 items-center gap-2 overflow-x-auto px-1 text-xs">
                            {pendingCount > 0 && <Button type="button" size="sm" variant="outline" className="shrink-0 border-amber-500/50"
                                onClick={() => jumpToPending()}>{t('pages.aiAssistant.workspace.pendingCount', { count: pendingCount })}</Button>}
                            {runningTaskCount > 0 && <Button type="button" size="sm" variant="ghost" className="shrink-0" onClick={() => setTaskPanelSession(permissionHistoryKey)}>
                                {t('pages.aiAssistant.workspace.runningTasks', { count: runningTaskCount })}</Button>}
                            {chat.goal && <Button type="button" size="sm" variant="ghost" className="max-w-[min(70vw,22rem)] shrink-0 truncate" onClick={() => setGoalDetailsOpen(true)}>
                                {t(`pages.aiAssistant.goalStates.${chat.goal.state}`)} · {chat.goal.goalText}</Button>}
                            {chat.approvalDelegation?.status === 'active' && <Button type="button" size="sm" variant="ghost" className="shrink-0" onClick={() => setApprovalSettingsOpen(true)}>
                                {t('pages.aiAssistant.workspace.autoApprovalActive')}</Button>}
                        </div>
                    )}
                    <div className="relative min-h-0 flex-1">
                    <div ref={scrollRef} onScroll={onScroll} data-testid="assistant-scroll-area"
                        className="assistant-scrollbar h-full overflow-y-auto overscroll-contain [overflow-wrap:anywhere]">
                    <div ref={contentRef} className="mx-auto w-full max-w-[840px] space-y-4 pb-4">
                    <div data-testid="ai-assistant-transcript" className="min-h-48 space-y-5 py-4">
                        {chat.hydrating && <Skeleton className="h-20 w-full" />}
                        {chat.hasMoreMessages && (
                            <div className="flex justify-center">
                                <Button
                                    type="button"
                                    size="sm"
                                    variant="ghost"
                                    disabled={chat.loadingOlderMessages}
                                    onClick={() => void chat.loadOlderMessages()}
                                >
                                    {chat.loadingOlderMessages && <LoaderCircle className="mr-2 h-4 w-4 animate-spin" />}
                                    {t('pages.aiAssistant.loadEarlierMessages')}
                                </Button>
                            </div>
                        )}
                        <AssistantDocumentPreviews previews={chat.documentPreviews}
                            requestPage={chat.requestDocumentPreviewPage} />
                        <AssistantImages key={chat.conversationId} sessionId={chat.sessionId} evidence={chat.visualEvidence}
                            messages={chat.messages} renderMessage={renderTranscriptMessage}
                            renderReasoning={message => <div className="w-full max-w-[90%] text-sm">
                                <AssistantReasoning text={message.reasoning} />
                            </div>}
                            renderToolGroup={messages => <AssistantToolGroup messages={messages} tools={chat.tools}
                                renderMessage={renderTranscriptMessage} displayNameForTool={displayNameForTool} />} />
                        <AssistantContextNotices historical notices={chat.contextNotices.filter(notice => !noticeMessageId(notice, chat.messages))} />
                        <ScheduleProposalCards key={`${deskId}:${chat.conversationId}`} tools={chat.tools} running={chat.running}
                            deviceId={stableDeviceId} connectionId={deskId} onPendingCountChange={setPendingScheduleCount} />
                        {chat.partial && (
                            <MarkdownContent disableLinks className="max-w-[90%] rounded-lg bg-muted px-3 py-2 text-sm">
                                {chat.partial}
                            </MarkdownContent>
                        )}
                    </div>

                    {externalSendReceipts.length > 0 && (
                        <div data-testid="ai-assistant-external-send-results" className="space-y-3">
                            {externalSendReceipts.map(({ tool, receipt }) => (
                                <div
                                    key={tool.callId}
                                    className={`space-y-1 rounded-md border p-3 ${
                                        receipt.outcome === 'sent'
                                            ? 'border-emerald-500/50 bg-emerald-500/5'
                                            : receipt.outcome === 'outcome_unknown'
                                                ? 'border-amber-500/50 bg-amber-500/5'
                                                : 'border-slate-500/40 bg-muted/30'
                                    }`}
                                >
                                    <p className="flex items-center gap-2 text-sm font-medium">
                                        {receipt.outcome === 'outcome_unknown' && <AlertTriangle className="h-4 w-4" />}
                                        {t(`pages.aiAssistant.externalSendResult.${receipt.outcome}`)}
                                    </p>
                                    <p className="text-xs text-muted-foreground">
                                        {t('pages.aiAssistant.externalSendResultDescription.' + receipt.outcome)}
                                    </p>
                                    <p className="break-all text-xs text-muted-foreground">
                                        {tool.name} · {new Date(receipt.observed_at_unix_ms).toLocaleString()}
                                        {receipt.provider_receipt_id ? ` · ${receipt.provider_receipt_id}` : ''}
                                    </p>
                                </div>
                            ))}
                        </div>
                    )}
                    <div data-assistant-pending={chat.pendingGoalOpenRequest ? '' : undefined} tabIndex={-1}>
                    {chat.pendingGoalOpenRequest && (
                        <div data-testid="ai-assistant-goal-open-request" className="rounded-md border border-amber-500/50 bg-amber-500/5 px-3 py-2 text-xs">
                            <p className="font-medium">{t(chat.pendingGoalOpenRequest.targetGoalId
                                ? 'pages.aiAssistant.goalRevisionTitle'
                                : 'pages.aiAssistant.goalProposalTitle')}</p>
                            <p className="mt-1 whitespace-pre-wrap break-words">{chat.pendingGoalOpenRequest.goalText}</p>
                            {chat.pendingGoalOpenRequest.previousCompletedGoalId && <p className="mt-1 text-muted-foreground">
                                {t('pages.aiAssistant.goalContinuesPrevious', { goalId: chat.pendingGoalOpenRequest.previousCompletedGoalId })}
                            </p>}
                            <p className="mt-1 text-muted-foreground">
                                {t(chat.pendingGoalOpenRequest.targetGoalId
                                    ? 'pages.aiAssistant.goalRevisionDetails'
                                    : 'pages.aiAssistant.goalProposalDetails', {
                                    device: chat.pendingGoalOpenRequest.deviceId,
                                    revision: chat.pendingGoalOpenRequest.targetGoalRevision,
                                    expiry: new Date(chat.pendingGoalOpenRequest.expiresAtUnixMs).toLocaleString(),
                                })}
                            </p>
                            {!chat.pendingGoalOpenRequest.targetGoalId && goalBudgetPolicy && <GoalLimitsSummary policy={goalBudgetPolicy} />}
                            <p className="mt-1 text-muted-foreground">{t(chat.pendingGoalOpenRequest.targetGoalId
                                ? 'pages.aiAssistant.goalRevisionBoundary'
                                : 'pages.aiAssistant.goalProposalBoundary')}</p>
                            {!offPageReminderAvailable && <p className="mt-1 text-amber-700 dark:text-amber-300">
                                {t('pages.aiAssistant.goalNoOffPageReminder')}
                            </p>}
                            <div className="mt-2 flex gap-2">
                                <Button size="sm" disabled={chat.goalOpenUpdating || chat.turnRunning
                                    || (!chat.pendingGoalOpenRequest.targetGoalId && !goalBudgetPolicy)
                                    || chat.pendingGoalOpenRequest.expiresAtUnixMs <= Date.now()}
                                    onClick={() => void chat.decideGoalOpen(true)}>
                                    {t(chat.pendingGoalOpenRequest.targetGoalId
                                        ? 'pages.aiAssistant.goalRevisionApprove'
                                        : 'pages.aiAssistant.goalProposalApprove')}
                                </Button>
                                <Button size="sm" variant="outline" disabled={chat.goalOpenUpdating || chat.turnRunning
                                    || chat.pendingGoalOpenRequest.expiresAtUnixMs <= Date.now()}
                                    onClick={() => void chat.decideGoalOpen(false)}>
                                    {t('pages.aiAssistant.goalProposalDeny')}
                                </Button>
                            </div>
                        </div>
                    )}
                    </div>
                    {pendingDirectories.map(directory => <div key={directory.requestId} data-assistant-pending tabIndex={-1}>
                        <AssistantDirectoryApproval directory={directory} revision={chat.fileScope.revision}
                            disabled={!assistantEnabled || !isConnected || chat.hydrating}
                            busy={chat.contextUpdating} onUpdate={chat.updateDirectory} />
                    </div>)}
                    <AssistantBackgroundTasks key={`tasks:${permissionHistoryKey}`}
                        open={taskPanelSession === permissionHistoryKey}
                        onOpenChange={open => setTaskPanelSession(open ? permissionHistoryKey : null)}
                        commands={chat.commandTasks} providers={chat.backgroundTasks} tools={chat.tools}
                        connected={isConnected} canCancelProvider={featureProfile.background_task_cancel}
                        cancelling={chat.taskCancelling} onCancel={chat.cancelTask} />
                    <AssistantFileScope key={`directories:${permissionHistoryKey}`} scope={chat.fileScope}
                        deskId={deskId} sessionTargetId={chat.sessionTargetReady ? (chat.sessionTarget?.target_id ?? null) : undefined}
                        open={directorySession === permissionHistoryKey} onOpenChange={open => setDirectorySession(open ? permissionHistoryKey : null)}
                        disabled={!assistantEnabled || !isConnected || chat.hydrating || chat.contextUpdating} onUpdate={chat.updateDirectory}
                        showPendingActions={false} onPendingJump={id => jumpToPending(`assistant-directory-${id}`)} />
                    <div data-assistant-pending={pendingPermissionCount > 0 ? '' : undefined} tabIndex={-1}>
                    <AssistantPermissionRecords key={permissionHistoryKey} requests={chat.permissionRequests}
                        open={permissionHistorySession === permissionHistoryKey}
                        onOpenChange={(open) => setPermissionHistorySession(open ? permissionHistoryKey : null)}>
                            {(request) => (
                                <AssistantPermissionRequest key={`${permissionHistoryKey}:${request.requestId}:${request.inputRevision}`}
                                    request={request} canDecide={featureProfile.permission_decision && request.inputRevision === chat.inputRevision}
                                    disabled={!assistantEnabled || !isConnected || chat.hydrating || chat.turnRunning}
                                    busy={chat.permissionUpdating} waitingForTurn={chat.turnRunning} onDecide={chat.decidePermissionItems} />
                            )}
                    </AssistantPermissionRecords>
                    </div>
                    {featureProfile.exec_pty && Object.entries(exec.entries).map(([row, entry]) => {
                        const rowIndex = Number(row);
                        return (
                            <div key={row} data-testid="ai-assistant-exec">
                                <ExecLifecycle
                                    entry={entry}
                                    onApprove={() => exec.approve(rowIndex)}
                                    onReject={() => exec.reject(rowIndex)}
                                    onCancel={() => exec.cancel(rowIndex)}
                                    onDismiss={() => exec.dismiss(rowIndex)}
                                    ptyClient={exec.ptyClient(rowIndex)}
                                    approvalDisabled={!assistantEnabled}
                                />
                            </div>
                        );
                    })}
                    {chat.error && (
                        <Alert variant="destructive">
                            <AlertTitle>{t('pages.aiAssistant.chatErrorTitle')}</AlertTitle>
                            <AlertDescription>{chat.error === 'history_restore_failed' ? t('pages.aiAssistant.history.restoreError') : chat.error === 'selected_context_expired' ? t('pages.aiAssistant.selectedContextExpired') : chat.error}</AlertDescription>
                        </Alert>
                    )}
                    {rehearsal && <Alert><AlertDescription>{t('schedules.rehearsal.executionNote')}</AlertDescription></Alert>}
                    </div>
                    </div>
                    {showJumpToLatest && (
                        <Button type="button" variant="outline" size="icon" onClick={jumpToLatest}
                            className="assistant-jump-action absolute bottom-3 right-3 rounded-full bg-background shadow-md"
                            aria-label={t('pages.aiAssistant.scrollToLatest')}
                            title={t('pages.aiAssistant.scrollToLatest')}>
                            <ArrowDown className="h-4 w-4" />
                        </Button>
                    )}
                    </div>
                    <form onSubmit={submit} className="assistant-composer mx-auto w-full max-w-[840px] shrink-0 space-y-2 rounded-xl border bg-background p-3 shadow-sm">
                        {chat.deliveryState && <div role="status" className="flex items-center justify-between gap-2 text-sm">
                            <span>{t(chat.deliveryState === 'sending' ? 'pages.aiAssistant.deliverySending' : 'pages.aiAssistant.deliveryUnconfirmed')}</span>
                            {chat.deliveryState === 'unconfirmed' && <Button type="button" size="sm" variant="outline"
                                disabled={!isConnected || !chat.sessionTargetReady} onClick={() => void chat.retryDelivery()}>
                                {t('pages.aiAssistant.deliveryRetry')}
                            </Button>}
                        </div>}
                        {!rehearsal && startGoal && !offPageReminderAvailable && (
                            <p className="text-xs text-amber-700 dark:text-amber-300">{t('pages.aiAssistant.goalNoOffPageReminder')}</p>
                        )}
                        {!rehearsal && startGoal && previousCompletedGoalId && (
                            <p className="text-xs text-muted-foreground">
                                {t('pages.aiAssistant.goalContinuesPrevious', { goalId: previousCompletedGoalId })}
                            </p>
                        )}
                        <Textarea
                            value={question}
                            readOnly={!!rehearsal}
                            onChange={(event) => setQuestion(event.target.value)}
                            placeholder={t('pages.aiAssistant.questionPlaceholder')}
                            maxLength={16_384}
                            disabled={!assistantEnabled || !isConnected || chat.hydrating || chat.contextUpdating || !providerConfig?.api_key_set || !providerConfig?.model}
                            className="min-h-16 max-h-40 w-full resize-y rounded-md border-0 bg-background px-3 py-2 text-sm shadow-sm outline-none placeholder:text-muted-foreground focus-visible:ring-1 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50"
                        />
                        <div className="flex min-w-0 items-center gap-1">
                            {isMobile ? <>
                                <Button type="button" variant="ghost" size="icon" className="assistant-add-action h-11 w-11 shrink-0"
                                    aria-label={t('pages.aiAssistant.workspace.addContext')} onClick={() => setAddContextOpen(true)}>
                                    <Plus className="h-4 w-4" aria-hidden="true" />
                                </Button>
                                <Sheet open={addContextOpen} onOpenChange={setAddContextOpen}>
                                    <SheetContent side="bottom" className="max-h-[85dvh] overflow-y-auto rounded-t-xl px-4 pb-[calc(1rem+env(safe-area-inset-bottom))] pt-5">
                                        <SheetHeader><SheetTitle>{t('pages.aiAssistant.workspace.addContext')}</SheetTitle></SheetHeader>
                                        <div className="mt-4 space-y-2">
                                            <Button type="button" variant="ghost" className="min-h-11 w-full justify-start"
                                                onClick={() => { setAddContextOpen(false); setPanel('context'); }}>
                                                {t('pages.aiAssistant.workspace.addContext')}
                                            </Button>
                                            {!rehearsal && <Button type="button" variant="ghost" className="min-h-11 w-full justify-start gap-2"
                                                aria-pressed={startGoal} disabled={!assistantEnabled || chat.turnRunning || !!chat.deliveryState}
                                                onClick={() => { setStartGoal(!startGoal); setPreviousCompletedGoalId(null); }}>
                                                <span className="inline-flex h-4 w-4 shrink-0 items-center justify-center">{startGoal && <Check className="h-4 w-4" aria-hidden="true" />}</span>
                                                {t('pages.aiAssistant.goalStart')}
                                            </Button>}
                                            {!rehearsal && startGoal && goalBudgetPolicy && <GoalLimitsSummary policy={goalBudgetPolicy} />}
                                        </div>
                                    </SheetContent>
                                </Sheet>
                            </> : <DropdownMenu>
                                <DropdownMenuTrigger asChild>
                                    <Button type="button" variant="ghost" size="icon" className="assistant-add-action h-11 w-11 shrink-0"
                                        aria-label={t('pages.aiAssistant.workspace.addContext')}>
                                        <Plus className="h-4 w-4" aria-hidden="true" />
                                    </Button>
                                </DropdownMenuTrigger>
                                <DropdownMenuContent align="start" className="max-w-[min(90vw,20rem)]">
                                    <DropdownMenuItem onSelect={() => setPanel('context')}>
                                        {t('pages.aiAssistant.workspace.addContext')}
                                    </DropdownMenuItem>
                                    {!rehearsal && <DropdownMenuCheckboxItem checked={startGoal}
                                        disabled={!assistantEnabled || chat.turnRunning || !!chat.deliveryState}
                                        onCheckedChange={checked => { setStartGoal(checked); setPreviousCompletedGoalId(null); }}>
                                        {t('pages.aiAssistant.goalStart')}
                                    </DropdownMenuCheckboxItem>}
                                    {!rehearsal && startGoal && goalBudgetPolicy && <div className="px-2 py-1"><GoalLimitsSummary policy={goalBudgetPolicy} /></div>}
                                </DropdownMenuContent>
                            </DropdownMenu>}
                            {selectedContextCount > 0 && <Button type="button" size="sm" variant="secondary" className="min-w-0 max-w-[min(28vw,10rem)] truncate"
                                onClick={() => setPanel('context')}>
                                {t('pages.aiAssistant.workspace.contextCount', { count: selectedContextCount })}
                            </Button>}
                            {startGoal && <Button type="button" size="sm" variant="secondary" className="min-w-0 max-w-[min(28vw,10rem)] gap-1"
                                onClick={() => { setStartGoal(false); setPreviousCompletedGoalId(null); }}>
                                <span className="truncate">{t('pages.aiAssistant.goalStart')}</span><X className="h-3.5 w-3.5 shrink-0" aria-hidden="true" />
                            </Button>}
                            <div className="ml-auto flex shrink-0 items-center gap-1">
                                <AssistantContextMeter usage={chat.contextUsage} draft={question} />
                                {chat.turnRunning ? (
                                    <Button type="button" className="assistant-action assistant-primary-action" aria-label={t(chat.stopping ? 'pages.aiAssistant.stopping' : 'pages.aiAssistant.stop')} onClick={chat.stop} disabled={!chat.canStop || chat.stopping}>
                                        <LoaderCircle aria-hidden="true" className="h-4 w-4 shrink-0 animate-spin motion-reduce:animate-none" />
                                        <span className="assistant-action-label">{t(chat.stopping ? 'pages.aiAssistant.stopping' : 'pages.aiAssistant.stop')}</span>
                                    </Button>
                                ) : (
                                    <Button type="submit" className="assistant-action assistant-primary-action" aria-label={t(rehearsal ? 'schedules.rehearsal.begin' : 'pages.aiAssistant.send')} disabled={!!chat.deliveryState || !rehearsalCanStart || !assistantEnabled || !question.trim() || !isConnected || chat.hydrating || !chat.sessionTargetReady || chat.sessionTargetResolving || chat.contextUpdating || !providerConfig?.api_key_set || !providerConfig?.model || (startGoal && !goalBudgetPolicy)}>
                                        <Send className="h-4 w-4 shrink-0" />
                                        <span className="assistant-action-label">{t(rehearsal ? 'schedules.rehearsal.begin' : 'pages.aiAssistant.send')}</span>
                                    </Button>
                                )}
                            </div>
                        </div>
                    </form>
                    <AssistantAttachments sessionId={chat.sessionId} open={attachmentsOpen}
                        onOpenChange={setAttachmentsOpen} showTrigger={false} />
                    <Sheet open={goalDetailsOpen} onOpenChange={setGoalDetailsOpen}>
                        <SheetContent className="w-full overflow-y-auto sm:max-w-xl">
                            <SheetHeader><SheetTitle>{t('pages.aiAssistant.goalStart')}</SheetTitle></SheetHeader>
                    {chat.goal && (
                        <div data-testid="ai-assistant-goal" className="rounded-md border bg-muted/30 px-3 py-2 text-xs">
                            <div className="flex items-center justify-between gap-2">
                                <span className="min-w-0 truncate font-medium" title={chat.goal.goalText}>{chat.goal.goalText}</span>
                                <Badge variant="outline">{t(`pages.aiAssistant.goalStates.${chat.goal.state}`)}</Badge>
                            </div>
                            <p className="mt-1 text-muted-foreground">
                                {t('pages.aiAssistant.goalUsage', {
                                    slices: chat.goal.usedSlices,
                                    sliceLimit: goalBudgetPolicy?.limits.slices === null ? t('pages.aiAssistant.goalBudgetDisabled') : goalBudgetPolicy?.limits.slices ?? '…',
                                    tokens: chat.goal.usedModelTokens,
                                    tokenLimit: goalBudgetPolicy?.limits.modelTokens === null ? t('pages.aiAssistant.goalBudgetDisabled') : goalBudgetPolicy?.limits.modelTokens ?? '…',
                                })}
                            </p>
                            <p className="mt-1 text-muted-foreground">
                                {t('pages.aiAssistant.goalBudgetUsage', {
                                    modelCalls: chat.goal.usedModelCalls,
                                    modelCallLimit: goalBudgetPolicy?.limits.modelCalls === null ? t('pages.aiAssistant.goalBudgetDisabled') : goalBudgetPolicy?.limits.modelCalls ?? '…',
                                    toolCalls: chat.goal.usedToolCalls,
                                    toolCallLimit: goalBudgetPolicy?.limits.toolCalls === null ? t('pages.aiAssistant.goalBudgetDisabled') : goalBudgetPolicy?.limits.toolCalls ?? '…',
                                    activeMinutes: Math.floor(chat.goal.usedActiveTimeMs / 60_000),
                                    activeMinuteLimit: goalBudgetPolicy?.limits.activeTimeMs === null ? t('pages.aiAssistant.goalBudgetDisabled') : goalBudgetPolicy?.limits.activeTimeMs ? Math.floor(goalBudgetPolicy.limits.activeTimeMs / 60_000) : '…',
                                })}
                            </p>
                            <p className="mt-1 text-muted-foreground">
                                {t('pages.aiAssistant.goalIdentity', {
                                    revision: chat.goal.goalRevision,
                                    device: chat.goal.deviceId,
                                    deadline: goalBudgetPolicy?.limits.deadlineMs === null ? t('pages.aiAssistant.goalBudgetDisabled')
                                        : goalBudgetPolicy?.limits.deadlineMs ? new Date(chat.goal.createdAtUnixMs + goalBudgetPolicy.limits.deadlineMs).toLocaleString() : '…',
                                })}
                            </p>
                            {goalBudgetPolicy && <GoalLimitsSummary policy={goalBudgetPolicy} />}
                            {chat.goal.checkpointSummary && <p className="mt-1 line-clamp-2">{t('pages.aiAssistant.goalCheckpoint', { summary: chat.goal.checkpointSummary })}</p>}
                            {chat.goal.previousCompletedGoalId && <p className="mt-1 text-muted-foreground">
                                {t('pages.aiAssistant.goalContinuesPrevious', { goalId: chat.goal.previousCompletedGoalId })}
                                {chat.goal.previousCompletionSummary && ` · ${chat.goal.previousCompletionSummary}`}
                            </p>}
                            {chat.goal.statusReason && <p className="mt-1 text-muted-foreground">{t(`pages.aiAssistant.goalReasons.${chat.goal.statusReason}`, { defaultValue: chat.goal.statusReason })}</p>}
                            {chat.goal.nextAttemptUnixMs && <p className="mt-1 text-muted-foreground">
                                {t('pages.aiAssistant.goalNextAttempt', { time: new Date(chat.goal.nextAttemptUnixMs).toLocaleString() })}
                            </p>}
                            {!['completed', 'failed', 'cancelled'].includes(chat.goal.state) && (
                                <div className="mt-2 flex flex-wrap gap-2">
                                    {['paused', 'waiting_user'].includes(chat.goal.state)
                                        ? <Button size="sm" variant="outline" disabled={chat.goalUpdating || chat.turnRunning || Boolean(chat.pendingGoalOpenRequest)}
                                            onClick={() => void chat.controlGoal(chat.goal?.pauseReason === 'stalled' ? 'retry_stalled' : 'resume')}>
                                            {t(chat.goal.pauseReason === 'stalled' ? 'pages.aiAssistant.goalRetry' : 'pages.aiAssistant.goalResume')}
                                        </Button>
                                        : <Button size="sm" variant="outline" disabled={chat.goalUpdating || chat.turnRunning}
                                            onClick={() => void chat.controlGoal('pause')}>
                                            {t('pages.aiAssistant.goalPause')}
                                        </Button>}
                                    <Button size="sm" variant="destructive" disabled={chat.goalUpdating || chat.turnRunning}
                                        onClick={() => void chat.controlGoal('cancel')}>
                                        {t('pages.aiAssistant.goalCancel')}
                                    </Button>
                                </div>
                            )}
                            {chat.goal.state === 'completed' && !chat.pendingGoalOpenRequest && <Button type="button" size="sm" variant="outline" className="mt-2"
                                disabled={chat.turnRunning || !!chat.deliveryState}
                                onClick={() => {
                                    setStartGoal(true);
                                    setPreviousCompletedGoalId(chat.goal?.goalId ?? null);
                                    const draft = t('pages.aiAssistant.goalStillIncompletePrompt', { goal: chat.goal?.goalText ?? '' });
                                    setQuestion(new TextEncoder().encode(draft).length <= 16_384
                                        ? draft : t('pages.aiAssistant.goalStillIncompletePromptShort'));
                                }}>
                                {t('pages.aiAssistant.goalStillIncomplete')}
                            </Button>}
                        </div>
                    )}
                        </SheetContent>
                    </Sheet>
                    <Sheet open={approvalSettingsOpen} onOpenChange={setApprovalSettingsOpen}>
                        <SheetContent className="w-full overflow-y-auto sm:max-w-xl">
                            <SheetHeader><SheetTitle>{t('pages.aiAssistant.autoApprovalTitle')}</SheetTitle></SheetHeader>
                    {!rehearsal && featureProfile.approval_delegation && chat.conversationId && (
                        <div data-testid="ai-assistant-automatic-approval" className="rounded-md border px-3 py-2 text-xs">
                            <div className="flex items-center justify-between gap-3">
                                <div className="min-w-0">
                                    <p className="flex items-center gap-2 font-medium"><ShieldCheck className="h-4 w-4" />{t('pages.aiAssistant.autoApprovalTitle')}</p>
                                    <p className="mt-1 text-muted-foreground">{t('pages.aiAssistant.autoApprovalDescription')}</p>
                                </div>
                                <Button size="sm" variant="outline"
                                    disabled={!assistantEnabled || chat.hydrating || chat.approvalUpdating
                                        || (!(chat.approvalDelegation?.status === 'active')
                                            && (!chat.approvalModelReadiness?.available || chat.turnRunning))}
                                    onClick={() => void chat.setAutomaticApproval(chat.approvalDelegation?.status !== 'active')}>
                                    {chat.approvalDelegation?.status === 'active'
                                        ? t('pages.aiAssistant.autoApprovalDisable') : t('pages.aiAssistant.autoApprovalEnable')}
                                </Button>
                            </div>
                            {chat.approvalDelegation?.status === 'active'
                                ? <p className="mt-1 text-muted-foreground">{t('pages.aiAssistant.autoApprovalUsage', {
                                    reviews: chat.approvalDelegation.reviewsUsed,
                                    tokens: chat.approvalDelegation.tokensUsed,
                                })}</p>
                                : chat.approvalModelReadiness && !chat.approvalModelReadiness.available
                                    ? <p className="mt-1 text-amber-700 dark:text-amber-300">{t('pages.aiAssistant.autoApprovalUnavailable')}: {t(`pages.aiAssistant.approvalModelReason.${chat.approvalModelReadiness.reason ?? 'unknown'}`)}</p>
                                    : null}
                        </div>
                    )}
                        </SheetContent>
                    </Sheet>
                    <Sheet open={deviceSettingsOpen} onOpenChange={setDeviceSettingsOpen}>
                        <SheetContent className="w-full sm:max-w-md">
                            <SheetHeader><SheetTitle>{t('pages.aiAssistant.workspace.deviceSettings')}</SheetTitle></SheetHeader>
                            <p className="mt-4 text-sm">{t(assistantEnabled
                                ? 'pages.aiAssistant.workspace.assistantOnDeviceEnabled'
                                : 'pages.aiAssistant.workspace.assistantOnDeviceDisabled')}</p>
                            <p className="mt-2 text-sm text-muted-foreground">{t('pages.aiAssistant.workspace.deviceSettingsOwnerOnly')}</p>
                        </SheetContent>
                    </Sheet>
                    <AssistantSchedules key={permissionHistoryKey} sessionId={chat.sessionId ?? null} open={schedulesOpen} onOpenChange={setSchedulesOpen} deviceId={stableDeviceId} />
                </CardContent>
            </Card>
        </>
    );
}

export default function AiAssistantPage({
    featureProfile: suppliedFeatureProfile,
}: {
    featureProfile?: AiAssistantFeatureProfile | null;
}) {
    const { id: deskId } = useParams<{ id: string }>();
    const [searchParams] = useSearchParams();
    const { t } = useTranslation();
    const serverInfo = useQueryServerInfo({ query: { enabled: suppliedFeatureProfile === undefined } });
    const featureProfile = suppliedFeatureProfile === undefined
        ? serverInfo.data?.data?.ai_assistant ?? null
        : suppliedFeatureProfile;
    const restricted = useRestrictedSession(deskId);
    const { data: connections, isLoading } = useListConnections();
    const connection = connections?.find((item: any) => item.connection_id === deskId);

    if (suppliedFeatureProfile === undefined && serverInfo.isLoading) {
        return <div className="p-6"><Skeleton className="h-64 w-full" /></div>;
    }

    if (!hasAiAssistantBrowserEntry(featureProfile)) {
        return (
            <div className="mx-auto max-w-3xl p-6">
                <Alert>
                    <AlertTitle>{t('pages.aiAssistant.unavailableTitle')}</AlertTitle>
                    <AlertDescription>
                        {t('pages.aiAssistant.unavailableDescription')}
                    </AlertDescription>
                </Alert>
            </div>
        );
    }

    if (restricted.isRestricted) {
        return (
            <div className="mx-auto max-w-3xl p-6">
                <Alert variant="destructive">
                    <AlertTitle>{t('pages.aiAssistant.ownerOnlyTitle')}</AlertTitle>
                    <AlertDescription>{t('pages.aiAssistant.ownerOnly')}</AlertDescription>
                </Alert>
            </div>
        );
    }

    if (isLoading) {
        return <div className="p-6"><Skeleton className="h-64 w-full" /></div>;
    }

    if (!deskId || !connection) {
        return (
            <div className="mx-auto max-w-3xl p-6">
                <Alert variant="destructive">
                    <AlertTitle>{t('pages.deskDashboard.notFound')}</AlertTitle>
                    <AlertDescription>{t('pages.deskDashboard.notFoundDesc')}</AlertDescription>
                </Alert>
            </div>
        );
    }

    return (
        <div className="absolute inset-0 flex min-w-0 flex-col gap-2 overflow-hidden">
            {searchParams.has('rehearsal') ? <AiAssistantRehearsalGate
                rehearsalId={searchParams.get('rehearsal') ?? ''}
                deviceId={String(connection.device_id ?? connection.version_info.client_id ?? '')}
            >
                {row => <AiAssistantWorkspace
                    key={row.rehearsal_id}
                    rehearsal={row}
                    backTo={`/desk/${encodeURIComponent(deskId)}`}
                    deskId={deskId}
                    stableDeviceId={connection.version_info.client_id ?? connection.device_id ?? deskId}
                    localPairingAvailable={!connection.device_id}
                    featureProfile={featureProfile}
                    assistantEnabled={isAiAssistantEnabled(connection.version_info)}
                />}
            </AiAssistantRehearsalGate> : (
                <AiAssistantWorkspace
                    backTo={`/desk/${encodeURIComponent(deskId)}`}
                    initialConversationId={searchParams.get('conversation')}
                    deskId={deskId}
                    stableDeviceId={connection.version_info.client_id ?? connection.device_id ?? deskId}
                    localPairingAvailable={!connection.device_id}
                    featureProfile={featureProfile}
                    assistantEnabled={isAiAssistantEnabled(connection.version_info)}
                />
            )}
        </div>
    );
}
