import { AssistantToolCall } from './assistant-tool-call';
import { useFollowLatest } from '@/hooks/use-follow-latest';
import './assistant-responsive.css';
import { AssistantSchedules } from './assistant-schedules';
import { AssistantImages } from './assistant-images';
import { AssistantReasoning } from './assistant-reasoning';
import { AssistantBackgroundTasks } from './assistant-background-tasks';
import { ScheduleProposalCards } from '@/features/schedules/proposal-card';
import { AiAssistantIcon } from '@/components/ai-assistant-icon';
import { AssistantContextMeter } from './assistant-context-meter';
import { AssistantComposerTools } from './assistant-composer-tools';
import { AssistantFileScope } from './assistant-file-scope';
import { AssistantConnectionIcon } from './assistant-connection-icon';
import { AssistantCommandResult } from './assistant-command-result';
import { AssistantContextNotices, noticeMessageId } from './assistant-context-notices';
import { AssistantPermissionRequest } from './assistant-permission-request';
import { AssistantPermissionRecords } from './assistant-permission-records';
import { AssistantHistory } from './assistant-history';
import { capabilityDescriptionKey } from './assistant-capability-copy';
import { Fragment, type FormEvent, useEffect, useRef, useState } from 'react';
import { useNavigate, useParams, useSearchParams } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { AlertTriangle, ArrowDown, ArrowLeft, CalendarClock, MessageSquarePlus, Check, Copy, Eye, LoaderCircle, Monitor, Puzzle, RefreshCw, Send, ShieldCheck, X } from 'lucide-react';

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
import { useRestrictedSession } from './restricted-session';
import { useDeskSignaling } from './use-desk-signaling';
import { isDeviceAssistantEnabled } from './device-assistant-switch';
import {
    type ObservationEntry,
    type OwnerSelectableWindow,
    type SelectableApplication,
    type ObservationRoot,
    ownerSelectableWindows,
    useDeviceAssistantObservation,
} from './use-device-assistant-observation';
import { useDeviceAssistantChat, type RehearsalConversation } from './use-device-assistant-chat';
import { DeviceAssistantRehearsalGate } from './device-assistant-rehearsal-gate';
import { SessionTargetDialog } from './session-target-selection';
import { useDeviceAssistantCapabilities } from './use-device-assistant-capabilities';
import { AssistantCapabilityList } from './assistant-capability-list';
import { AssistantDetailsSheet, type AssistantPanelId } from './assistant-details-sheet';
import { requiresBrowserRemoteTakeover } from './device-assistant-browser-takeover';
import { useConfirmExec } from '../exec/use-confirm-exec';
import { ExecLifecycle } from '../exec/exec-lifecycle';
import {
    type DeviceAssistantFeatureProfile,
    OSS_DEVICE_ASSISTANT_FEATURES,
    hasDeviceAssistantBrowserEntry,
} from './device-assistant-features';
import {
    isExactExternalSendTool,
    parseExternalSendReceipt,
} from './device-assistant-external-send';

const CURRENT_SCREEN_CAPABILITY_ID = 'screen.capture.current';

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
                        {t(`pages.deviceAssistant.phase.${entry.phase}`)}
                    </Badge>
                </div>
            </CardHeader>
            <CardContent className="space-y-3">
                <Button variant="outline" size="sm" onClick={onRefresh} disabled={disabled || isPending || isScheduled}>
                    {isPending
                        ? <LoaderCircle className="mr-2 h-4 w-4 animate-spin" />
                        : <RefreshCw className="mr-2 h-4 w-4" />}
                    {t('pages.deviceAssistant.refresh')}
                </Button>
                {onDelayedRefresh && !isScheduled && (
                    <Button className="ml-2" variant="outline" size="sm" onClick={onDelayedRefresh} disabled={disabled || isPending}>
                        {t('pages.deviceAssistant.delayedObserve')}
                    </Button>
                )}
                {isScheduled && (
                    <div role="status" className="space-y-2 text-sm">
                        <p>{t('pages.deviceAssistant.observationCountdown', { count: remainingSeconds })}</p>
                        <Button variant="outline" size="sm" onClick={onCancelDelayed}>
                            {t('pages.deviceAssistant.cancelObservation')}
                        </Button>
                    </div>
                )}
                {onListApplications && (
                    <div className="space-y-2 rounded-md border p-3">
                        <Button variant="outline" size="sm" onClick={onListApplications} disabled={disabled || isPending || isScheduled}>
                            {t('pages.deviceAssistant.listApplications')}
                        </Button>
                        <p className="text-xs text-muted-foreground">{t('pages.deviceAssistant.applicationSelectionHint')}</p>
                        <div className="flex flex-wrap gap-2">
                            {applications.map((app) => <Button key={app.objectRef.token} variant="outline" size="sm"
                                disabled={disabled || isPending || isScheduled || Date.parse(app.objectRef.expires_at) <= Date.now()}
                                onClick={() => onInspectApplication?.(app.objectRef)}>
                                {app.name || t('pages.deviceAssistant.applicationUnnamed')}
                            </Button>)}
                        </div>
                    </div>
                )}
                {error && (
                    <Alert variant="destructive">
                        <AlertTitle>{error.kind}</AlertTitle>
                        <AlertDescription>{error.message}</AlertDescription>
                        {error.kind === 'permission_denied' && error.message.includes('allowlist') && (
                            <AlertDescription>{t('pages.deviceAssistant.applicationRestrictionHint')}</AlertDescription>
                        )}
                    </Alert>
                )}
                {windowCandidates.length > 0 && onAttachWindow && (
                    <div className="space-y-2 rounded-md border p-3">
                        <div>
                            <p className="text-sm font-medium">
                                {t('pages.deviceAssistant.windowSelectorTitle')}
                            </p>
                            <p className="text-xs text-muted-foreground">
                                {t('pages.deviceAssistant.windowSelectorDescription')}
                            </p>
                        </div>
                        <div className="flex flex-wrap gap-2">
                            {windowCandidates.map((candidate) => {
                                const label = candidate.title
                                    ?? t('pages.deviceAssistant.windowSelectorUntitled');
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
                    <pre
                        data-testid="observation-output"
                        className="max-h-80 overflow-auto whitespace-pre-wrap break-words rounded-md bg-muted p-3 text-xs"
                    >
                        {JSON.stringify(entry.outcome.data, null, 2)}
                    </pre>
                )}
            </CardContent>
        </Card>
    );
}

export function DeviceAssistantWorkspace({
    rehearsal,
    deskId,
    stableDeviceId,
    localPairingAvailable,
    featureProfile,
    assistantEnabled,
    onBrowserTakeover,
}: {
    rehearsal?: RehearsalConversation;
    deskId: string;
    stableDeviceId: string;
    localPairingAvailable: boolean;
    featureProfile: DeviceAssistantFeatureProfile;
    assistantEnabled: boolean;
    onBrowserTakeover: () => void;
}) {
    const { t } = useTranslation();
    const { i18n } = useTranslation();
    const { isConnected, subscribe, sendMessage } = useDeskSignaling();
    const { entries, inspectSession, inspectUi, scheduleUi, cancelDelayedUi, remainingSeconds, applications, listApplications, applicationSelectionAvailable } = useDeviceAssistantObservation({
        deskId,
        enabled: assistantEnabled && isConnected,
        subscribe,
        sendMessage,
    });
    const scheduleNavigate = useNavigate();
    const chat = useDeviceAssistantChat({
        rehearsal,
        deskId,
        connected: isConnected,
        conversationStorageScope: stableDeviceId,
        subscribe,
        sendMessage,
    });
    const capabilities = useDeviceAssistantCapabilities({ deskId, subscribe, sendMessage });
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
    const [panel, setPanel] = useState<AssistantPanelId | null>(null);
    const [permissionHistorySession, setPermissionHistorySession] = useState<string | null>(null);
    const [directorySession, setDirectorySession] = useState<string | null>(null);
    const permissionHistoryKey = `${deskId}:${chat.conversationId}`;
    const { scrollRef, contentRef, onScroll, showJumpToLatest, jumpToLatest } = useFollowLatest(true, permissionHistoryKey);
    const pendingDirectoryKey = chat.fileScope.directories.filter(directory => directory.state === 'pending')
        .map(directory => directory.requestId).join(':');
    useEffect(() => {
        if (pendingDirectoryKey) setDirectorySession(permissionHistoryKey);
    }, [pendingDirectoryKey, permissionHistoryKey]);
    useEffect(() => { setPermissionHistorySession(null); }, [permissionHistoryKey]);
    const [selectedCapabilityIds, setSelectedCapabilityIds] = useState<string[]>([]);
    const started = useRef(false);

    useEffect(() => {
        if (!assistantEnabled || !isConnected || started.current) return;
        started.current = true;
        capabilities.refresh();
    }, [assistantEnabled, capabilities.refresh, isConnected]);

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

    const submit = (event: FormEvent) => {
        event.preventDefault();
        if (chat.turnRunning) return;
        if (!assistantEnabled || !rehearsalCanStart) return;
        const selectedContext = featureProfile.object_context ? selectedCapabilityIds : [];
        if (chat.start(question, i18n.language, selectedContext)) {
            setQuestion('');
            setSelectedCapabilityIds((current) =>
                current.filter((id) => id !== CURRENT_SCREEN_CAPABILITY_ID),
            );
        }
    };

    const resetConversation = () => {
        chat.reset();
        setSelectedCapabilityIds([]);
    };

    const detailsContent = (
        <div className="space-y-4">
                                {chat.taskStatusProjection && (
                        <div data-testid="device-assistant-task-status" className="space-y-2 rounded-md border p-3">
                            <div className="flex flex-wrap items-center justify-between gap-2">
                                <div>
                                    <p className="text-sm font-medium">{t('pages.deviceAssistant.taskStatusTitle')}</p>
                                    <p className="text-xs text-muted-foreground">
                                        {t('pages.deviceAssistant.taskStatusDescription')}
                                    </p>
                                </div>
                                <div className="flex gap-2">
                                    {chat.pendingInputCount > 0 && (
                                        <Badge variant="secondary">
                                            {t('pages.deviceAssistant.pendingInputs', { count: chat.pendingInputCount })}
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
                                            {t(`pages.deviceAssistant.taskStatus.${item.status}`)}
                                        </Badge>
                                    </div>
                                ))}
                            </div>
                        </div>
                    )}

                                {chat.capabilityGrants.length > 0 && (
                        <div data-testid="device-assistant-capability-grants" className="space-y-3 rounded-md border border-emerald-500/40 p-3">
                            <div>
                                <p className="flex items-center gap-2 text-sm font-medium">
                                    <ShieldCheck className="h-4 w-4" />
                                    {t('pages.deviceAssistant.grantTitle')}
                                </p>
                                <p className="text-xs text-muted-foreground">
                                    {t('pages.deviceAssistant.grantDescription')}
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
                                                    {grant.toolName}
                                                </p>
                                                <p className="break-all text-xs text-muted-foreground">
                                                    {grant.providerId} · {grant.capabilityId} · {grant.riskTier}
                                                </p>
                                            </div>
                                            <Badge variant={state === 'active' ? 'default' : 'outline'}>
                                                {t(`pages.deviceAssistant.grantState.${state}`)}
                                            </Badge>
                                        </div>
                                        <div className="space-y-1 text-xs text-muted-foreground">
                                            <p>{t('pages.deviceAssistant.grantRemainingUses', { count: grant.remainingUses })}</p>
                                            <p>{t('pages.deviceAssistant.grantExpiresAt', {
                                                time: new Date(grant.expiresAtUnixMs).toLocaleString(),
                                            })}</p>
                                            {[...grant.resourceScope, ...grant.operationScope].length > 0 && (
                                                <p className="break-all">
                                                    {[...grant.resourceScope, ...grant.operationScope].join(' · ')}
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
                                                {t('pages.deviceAssistant.grantRevoke')}
                                            </Button>
                                        )}
                                    </div>
                                );
                            })}
                        </div>
                    )}

            {chat.tools.length === 0 && <p className="text-sm text-muted-foreground">{t('pages.deviceAssistant.workspace.emptyActivity')}</p>}
            {chat.tools.map((tool) => (
                <details key={tool.callId} className="rounded-lg border p-3">
                    <summary className="cursor-pointer text-sm">{tool.name} · {t(`pages.deviceAssistant.workspace.toolState.${tool.status}`)}</summary>
                    <p className="mt-2 text-sm">{tool.permissionReason && t('pages.deviceAssistant.permissionReasonLabel', { reason: tool.permissionReason })}</p>
                    <pre className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap break-words text-xs">{tool.argumentsJson}</pre>
                    {tool.output && <pre className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap break-words text-xs">{tool.output}</pre>}
                </details>
            ))}
                                {chat.draft && (
                        <Card data-testid="computer-action-draft-preview" className="border-violet-500/40">
                            <CardHeader>
                                <CardTitle className="text-base">{t('pages.deviceAssistant.draftTitle')}</CardTitle>
                                <CardDescription>
                                    {t('pages.deviceAssistant.draftDescription', {
                                        count: chat.draft.actions.length,
                                        risk: chat.draft.risk,
                                    })}
                                </CardDescription>
                            </CardHeader>
                            <CardContent className="space-y-3">
                                <pre className="max-h-64 overflow-auto whitespace-pre-wrap break-words rounded-md bg-muted p-3 text-xs">
                                    {JSON.stringify(chat.draft, null, 2)}
                                </pre>
                                <Button disabled>{t('pages.deviceAssistant.executionDisabled')}</Button>
                            </CardContent>
                        </Card>
                    )}

        </div>
    );

    return (
        <>
            <AssistantDetailsSheet panel={panel} onPanelChange={setPanel} sections={{
                details: detailsContent,
                capabilities: <AssistantCapabilityList entries={capabilities.snapshot?.entries ?? []}
                    loading={capabilities.loading} error={Boolean(capabilities.error)}
                    refreshDisabled={!assistantEnabled || !isConnected} onRefresh={capabilities.refresh} />,
                context: <>            {featureProfile.object_context && (
            <Card data-testid="device-assistant-context-selector">
                <CardHeader>
                    <CardTitle className="text-base">{t('pages.deviceAssistant.contextTitle')}</CardTitle>
                    <CardDescription>{t('pages.deviceAssistant.contextDescription')}</CardDescription>
                </CardHeader>
                <CardContent className="space-y-2">
                    {contextCapabilities.length === 0 && (
                        <p className="text-sm text-muted-foreground">
                            {t('pages.deviceAssistant.contextEmpty')}
                        </p>
                    )}
                    {contextCapabilities.map((entry) => {
                        const id = entry.capability.capability_id;
                        const selected = selectedCapabilityIds.includes(id);
                        return (
                            <button
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
                                            defaultValue: t('pages.deviceAssistant.workspace.descriptionUnavailable'),
                                        })}
                                    </span>
                                    <span className="block text-xs text-muted-foreground">
                                        {entry.ready
                                            ? t('pages.deviceAssistant.contextWillSend')
                                            : entry.reason ?? t('pages.deviceAssistant.contextUnavailable')}
                                    </span>
                                </span>
                                <Badge variant={selected ? 'default' : 'outline'}>
                                    {selected && <Check className="mr-1 h-3 w-3" />}
                                    {selected
                                        ? t('pages.deviceAssistant.contextSelected')
                                        : t('pages.deviceAssistant.contextNotSelected')}
                                </Badge>
                            </button>
                        );
                    })}
                    {chat.attachments.length > 0 && (
                        <div className="space-y-2 border-t pt-3" data-testid="device-assistant-attachments">
                            <p className="text-xs font-medium text-muted-foreground">
                                {t('pages.deviceAssistant.attachmentTitle')}
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
                                                ? t('pages.deviceAssistant.attachmentActive')
                                                : t('pages.deviceAssistant.attachmentStale', {
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
                                                title={t('pages.deviceAssistant.attachmentDetach')}
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
                connection: <>            <Alert>
                <ShieldCheck className="h-4 w-4" />
                <AlertTitle>{t('pages.deviceAssistant.disclosureTitle')}</AlertTitle>
                <AlertDescription className="whitespace-pre-line">{t('pages.deviceAssistant.disclosure')}</AlertDescription>
            </Alert>
            {localPairingAvailable && (
                <Card data-testid="browser-extension-pairing">
                    <CardHeader>
                        <CardTitle className="flex items-center gap-2 text-base">
                            <Puzzle className="h-4 w-4" />
                            {t('pages.deviceAssistant.browserExtensionTitle')}
                        </CardTitle>
                        <CardDescription>
                            {t('pages.deviceAssistant.browserExtensionDescription')}
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
                                {t('pages.deviceAssistant.browserExtensionShowCode')}
                            </Button>
                        )}
                        {browserPairing.isError && (
                            <Alert variant="destructive">
                                <AlertDescription>
                                    {t('pages.deviceAssistant.browserExtensionUnavailable')}
                                </AlertDescription>
                            </Alert>
                        )}
                        {pairing && (
                            <div className="space-y-2">
                                <div className="flex gap-2">
                                    <Input
                                        aria-label={t('pages.deviceAssistant.browserExtensionPairingCode')}
                                        readOnly
                                        value={pairing.pairing_code}
                                        className="font-mono text-xs"
                                    />
                                    <Button
                                        variant="outline"
                                        size="icon"
                                        aria-label={t('pages.deviceAssistant.browserExtensionCopyCode')}
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
                                    {t('pages.deviceAssistant.browserExtensionBridge', {
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
                    title={t('pages.deviceAssistant.sessionTitle')}
                    description={t('pages.deviceAssistant.sessionDescription')}
                    entry={entries.desktop_session_inspect}
                    onRefresh={() => inspectSession()}
                    disabled={!assistantEnabled || !isConnected}
                />
                <ObservationCard
                    title={t('pages.deviceAssistant.uiTitle')}
                    description={t('pages.deviceAssistant.uiDescription')}
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
                        candidate.title ?? t('pages.deviceAssistant.windowSelectorUntitled'),
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
                <Alert data-testid="device-assistant-disabled">
                    <AlertTitle>{t('pages.deviceAssistant.disabledTitle')}</AlertTitle>
                    <AlertDescription>{t('pages.deviceAssistant.disabledDescription')}</AlertDescription>
                </Alert>
            )}
            {[
                featureProfile.permission_decision,
                featureProfile.grant_revoke,
                featureProfile.background_task_cancel,
                featureProfile.object_context,
            ].some((enabled) => !enabled) && (
                <Alert data-testid="device-assistant-partial-support">
                    <AlertTitle>{t('pages.deviceAssistant.partialSupportTitle')}</AlertTitle>
                    <AlertDescription>
                        {t('pages.deviceAssistant.partialSupportDescription')}
                    </AlertDescription>
                </Alert>
            )}
            {browserTakeoverRequired && (
                <Card data-testid="browser-remote-takeover">
                    <CardHeader>
                        <CardTitle className="flex items-center gap-2 text-base">
                            <Monitor className="h-4 w-4" />
                            {t('pages.deviceAssistant.browserTakeoverTitle')}
                        </CardTitle>
                        <CardDescription>
                            {t('pages.deviceAssistant.browserTakeoverDescription')}
                        </CardDescription>
                    </CardHeader>
                    <CardContent className="space-y-2">
                        <Button
                            variant="outline"
                            disabled={!assistantEnabled || chat.running}
                            onClick={onBrowserTakeover}
                        >
                            {t('pages.deviceAssistant.browserTakeoverAction')}
                        </Button>
                        {chat.running && (
                            <p className="text-xs text-muted-foreground">
                                {t('pages.deviceAssistant.browserTakeoverBusy')}
                            </p>
                        )}
                    </CardContent>
                </Card>
            )}
            <Card className="mx-auto flex min-h-0 w-full max-w-4xl flex-1 flex-col border-0 shadow-none">
                <CardHeader className="assistant-header shrink-0 px-0 py-3">
                    <div data-testid="assistant-title-row" className="flex items-center justify-between gap-2">
                        <div className="min-w-0 flex-1">
                            <CardTitle className="flex min-w-0 items-center gap-2 text-base">
                                <AssistantConnectionIcon connected={isConnected} enabled={assistantEnabled} />
                                <span title={chat.sessionTarget?.display_name} className="truncate">{t('pages.deviceAssistant.chatTitle')}</span>
                            </CardTitle>
                        </div>
                        <div className="flex shrink-0 items-center gap-1">
                            <AssistantHistory deskId={deskId} disabled={!!rehearsal || chat.hydrating || chat.contextUpdating || chat.permissionUpdating || !!chat.grantRevoking}
                                onDeleted={id => { if (chat.forgetConversation(id)) setSelectedCapabilityIds([]); }}
                                onSelect={(id) => {
                                    if (!chat.selectConversation(id)) return false;
                                    setQuestion('');
                                    setSelectedCapabilityIds([]);
                                    return true;
                                }} />
                            {!rehearsal && <Button variant="ghost" size="sm" className="assistant-action" aria-label={t('schedules.createResume')} title={t('schedules.createResume')} disabled={!assistantEnabled || chat.running || chat.hydrating || !chat.conversationId || !chat.inputRevision} onClick={() => { if (!chat.conversationId || !chat.inputRevision) return; scheduleNavigate(`/schedules?${new URLSearchParams({ resume_conversation: chat.conversationId, resume_device: stableDeviceId, resume_revision: String(chat.inputRevision) })}`); }}><CalendarClock className="h-4 w-4 shrink-0" aria-hidden="true" /><span className="assistant-action-label">{t('schedules.createResume')}</span></Button>}
                            <Button variant="ghost" size="sm" className="assistant-action" aria-label={t('pages.deviceAssistant.newConversation')} title={t('pages.deviceAssistant.newConversation')} onClick={resetConversation} disabled={!!rehearsal || !assistantEnabled || chat.hydrating || chat.contextUpdating || chat.permissionUpdating || !!chat.grantRevoking}>
                                <MessageSquarePlus className="h-4 w-4 shrink-0" aria-hidden="true" /><span className="assistant-action-label">{t('pages.deviceAssistant.newConversation')}</span>
                            </Button>
                        </div>
                    </div>

                    <CardDescription>
                        {t('pages.deviceAssistant.providerBoundary', {
                            provider: providerConfig?.wire_protocol ?? t('pages.deviceAssistant.providerUnknown'),
                            model: providerConfig?.model ?? t('pages.deviceAssistant.providerUnknown'),
                        })}
                    </CardDescription>
                </CardHeader>
                <CardContent className="flex min-h-0 flex-1 flex-col gap-3 p-0">
                    <div className="relative min-h-0 flex-1">
                    <div ref={scrollRef} onScroll={onScroll} data-testid="assistant-scroll-area"
                        className="h-full overflow-y-auto overscroll-contain [overflow-wrap:anywhere]">
                    <div ref={contentRef} className="space-y-4 pb-4">
                    <div data-testid="device-assistant-transcript" className="min-h-48 space-y-5 py-4">
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
                                    {t('pages.deviceAssistant.loadEarlierMessages')}
                                </Button>
                            </div>
                        )}
                        <ScheduleProposalCards key={`${deskId}:${chat.conversationId}`} tools={chat.tools} running={chat.running} deviceId={stableDeviceId} connectionId={deskId} />
                        <AssistantImages key={chat.conversationId} sessionId={chat.sessionId} evidence={chat.visualEvidence}
                            messages={chat.messages} renderMessage={(message) => (
                            <Fragment key={message.id}>
                            <div
                                key={message.id}
                                className={`max-w-[90%] rounded-lg px-3 py-2 text-sm ${
                                    message.role === 'user'
                                        ? 'ml-auto bg-muted'
                                        : message.role === 'tool_result' ? 'w-full border bg-muted/30' : 'w-full bg-transparent'
                                }`}
                            >
                                {message.role === 'tool_call' ? <AssistantToolCall tool={chat.tools.find(tool => tool.callId === message.toolCallId)} running={chat.running} /> : message.role === 'tool_result' ? <><p className="mb-2 text-sm">{message.permissionReason && t('pages.deviceAssistant.permissionReasonLabel', { reason: message.permissionReason })}</p><AssistantCommandResult text={message.text} /></> : message.role === 'assistant'
                                    ? <><AssistantReasoning text={message.reasoning} />{message.text && <MarkdownContent disableLinks>{message.text}</MarkdownContent>}</>
                                    : <p className="whitespace-pre-wrap">{message.text}</p>}
                            </div>
                            <AssistantContextNotices notices={chat.contextNotices.filter(notice => noticeMessageId(notice, chat.messages) === message.id)} />
                            </Fragment>
                        )} />
                        <AssistantContextNotices historical notices={chat.contextNotices.filter(notice => !noticeMessageId(notice, chat.messages))} />
                        {chat.partial && (
                            <MarkdownContent disableLinks className="max-w-[90%] rounded-lg bg-muted px-3 py-2 text-sm">
                                {chat.partial}
                            </MarkdownContent>
                        )}
                    </div>

                    {externalSendReceipts.length > 0 && (
                        <div data-testid="device-assistant-external-send-results" className="space-y-3">
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
                                        {t(`pages.deviceAssistant.externalSendResult.${receipt.outcome}`)}
                                    </p>
                                    <p className="text-xs text-muted-foreground">
                                        {t('pages.deviceAssistant.externalSendResultDescription.' + receipt.outcome)}
                                    </p>
                                    <p className="break-all text-xs text-muted-foreground">
                                        {tool.name} · {new Date(receipt.observed_at_unix_ms).toLocaleString()}
                                        {receipt.provider_receipt_id ? ` · ${receipt.provider_receipt_id}` : ''}
                                    </p>
                                </div>
                            ))}
                        </div>
                    )}
                    <AssistantBackgroundTasks key={`tasks:${permissionHistoryKey}`}
                        open={taskPanelSession === permissionHistoryKey}
                        onOpenChange={open => setTaskPanelSession(open ? permissionHistoryKey : null)}
                        commands={chat.commandTasks} providers={chat.backgroundTasks} tools={chat.tools}
                        connected={isConnected} canCancelProvider={featureProfile.background_task_cancel}
                        cancelling={chat.taskCancelling} onCancel={chat.cancelTask} />
                    <AssistantFileScope key={`directories:${permissionHistoryKey}`} scope={chat.fileScope}
                        open={directorySession === permissionHistoryKey} onOpenChange={open => setDirectorySession(open ? permissionHistoryKey : null)}
                        disabled={!assistantEnabled || !isConnected || chat.hydrating || chat.contextUpdating} onUpdate={chat.updateDirectory} />
                    <AssistantPermissionRecords key={permissionHistoryKey} requests={chat.permissionRequests}
                        open={permissionHistorySession === permissionHistoryKey}
                        onOpenChange={(open) => setPermissionHistorySession(open ? permissionHistoryKey : null)}>
                            {(request) => (
                                <AssistantPermissionRequest key={`${permissionHistoryKey}:${request.requestId}:${request.inputRevision}`}
                                    request={request} canDecide={featureProfile.permission_decision}
                                    disabled={!assistantEnabled || !isConnected || chat.hydrating}
                                    busy={chat.permissionUpdating} onDecide={chat.decidePermissionItems} />
                            )}
                    </AssistantPermissionRecords>
                    {featureProfile.exec_pty && Object.entries(exec.entries).map(([row, entry]) => {
                        const rowIndex = Number(row);
                        return (
                            <div key={row} data-testid="device-assistant-exec">
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
                            <AlertTitle>{t('pages.deviceAssistant.chatErrorTitle')}</AlertTitle>
                            <AlertDescription>{chat.error === 'history_restore_failed' ? t('pages.deviceAssistant.history.restoreError') : chat.error}</AlertDescription>
                        </Alert>
                    )}
                    {rehearsal && <Alert><AlertDescription>{t('schedules.rehearsal.executionNote')}</AlertDescription></Alert>}
                    </div>
                    </div>
                    {showJumpToLatest && (
                        <Button type="button" variant="outline" size="icon" onClick={jumpToLatest}
                            className="absolute bottom-3 right-3 rounded-full bg-background shadow-md"
                            aria-label={t('pages.deviceAssistant.scrollToLatest')}
                            title={t('pages.deviceAssistant.scrollToLatest')}>
                            <ArrowDown className="h-4 w-4" />
                        </Button>
                    )}
                    </div>
                    <form onSubmit={submit} className="assistant-composer shrink-0 space-y-2 rounded-xl border bg-background p-3 shadow-sm">
                        <div className="flex flex-wrap items-center gap-2">
                            <Button type="button" size="sm" variant="ghost" onClick={() => setPanel('context')}>
                                {t('pages.deviceAssistant.workspace.addContext')}
                            </Button>
                            <span className="text-xs text-muted-foreground">{t('pages.deviceAssistant.workspace.contextCount', {
                                count: new Set([...selectedCapabilityIds, ...chat.attachments.filter((item) => item.state === 'active').map((item) => item.capabilityId)]).size,
                            })}</span>
                        </div>
                        <textarea
                            value={question}
                            readOnly={!!rehearsal}
                            onChange={(event) => setQuestion(event.target.value)}
                            placeholder={t('pages.deviceAssistant.questionPlaceholder')}
                            maxLength={16_384}
                            disabled={!assistantEnabled || !isConnected || chat.hydrating || chat.contextUpdating || !providerConfig?.api_key_set || !providerConfig?.model}
                            className="min-h-16 max-h-40 w-full resize-y rounded-md border-0 bg-background px-3 py-2 text-sm shadow-sm outline-none placeholder:text-muted-foreground focus-visible:ring-1 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50"
                        />
                        <div className="flex flex-wrap items-center justify-between gap-1">
                            <AssistantComposerTools
                                meter={<AssistantContextMeter usage={chat.contextUsage} draft={question} />}
                                onTasks={() => setTaskPanelSession(permissionHistoryKey)}
                                runningTaskCount={[...chat.commandTasks, ...chat.backgroundTasks].filter(task => ['running', 'cancel_requested'].includes(task.state)).length}
                                onDetails={() => setPanel('details')}
                                onPermissionHistory={() => setPermissionHistorySession(permissionHistoryKey)}
                                onDirectories={() => setDirectorySession(permissionHistoryKey)}
                                onSchedules={() => setSchedulesOpen(true)}
                            />
                            <div className="ml-auto flex shrink-0 items-center gap-1">
                            {chat.turnRunning ? (
                                <Button type="button" className="assistant-action" aria-label={t(chat.stopping ? 'pages.deviceAssistant.stopping' : 'pages.deviceAssistant.stop')} onClick={chat.stop} disabled={!chat.canStop || chat.stopping}>
                                    <LoaderCircle aria-hidden="true" className="h-4 w-4 shrink-0 animate-spin motion-reduce:animate-none" />
                                    <span className="assistant-action-label">{t(chat.stopping ? 'pages.deviceAssistant.stopping' : 'pages.deviceAssistant.stop')}</span>
                                </Button>
                            ) : (
                                <Button type="submit" className="assistant-action" aria-label={t(rehearsal ? 'schedules.rehearsal.begin' : 'pages.deviceAssistant.send')} disabled={!rehearsalCanStart || !assistantEnabled || !question.trim() || !isConnected || chat.hydrating || !chat.sessionTargetReady || chat.sessionTargetResolving || chat.contextUpdating || !providerConfig?.api_key_set || !providerConfig?.model}>
                                    <Send className="h-4 w-4 shrink-0" />
                                    <span className="assistant-action-label">{t(rehearsal ? 'schedules.rehearsal.begin' : 'pages.deviceAssistant.send')}</span>
                                </Button>
                            )}
                        </div>
                        </div>
                    </form>
                    <AssistantSchedules key={permissionHistoryKey} sessionId={chat.sessionId ?? null} open={schedulesOpen} onOpenChange={setSchedulesOpen} deviceId={stableDeviceId} />
                </CardContent>
            </Card>
        </>
    );
}

export default function DeviceAssistantPage({
    featureProfile = OSS_DEVICE_ASSISTANT_FEATURES,
}: {
    featureProfile?: DeviceAssistantFeatureProfile | null;
}) {
    const { id: deskId } = useParams<{ id: string }>();
    const navigate = useNavigate();
    const [searchParams] = useSearchParams();
    const { t } = useTranslation();
    const restricted = useRestrictedSession(deskId);
    const { data: connections, isLoading } = useListConnections();
    const connection = connections?.find((item: any) => item.connection_id === deskId);

    if (!hasDeviceAssistantBrowserEntry(featureProfile)) {
        return (
            <div className="mx-auto max-w-3xl p-6">
                <Alert>
                    <AlertTitle>{t('pages.deviceAssistant.unavailableTitle')}</AlertTitle>
                    <AlertDescription>
                        {t('pages.deviceAssistant.unavailableDescription')}
                    </AlertDescription>
                </Alert>
            </div>
        );
    }

    if (restricted.isRestricted) {
        return (
            <div className="mx-auto max-w-3xl p-6">
                <Alert variant="destructive">
                    <AlertTitle>{t('pages.deviceAssistant.ownerOnlyTitle')}</AlertTitle>
                    <AlertDescription>{t('pages.deviceAssistant.ownerOnly')}</AlertDescription>
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
        <div className="absolute inset-0 mx-auto flex max-w-6xl flex-col gap-3 overflow-hidden p-3 sm:p-6">
            <div className="flex shrink-0 items-center gap-4">
                <Button variant="outline" size="icon" onClick={() => navigate(`/desk/${deskId}`)}>
                    <ArrowLeft className="h-4 w-4" />
                </Button>
                <div>
                    <h1 className="flex items-center gap-2 text-2xl font-bold">
                        <AiAssistantIcon className="h-6 w-6 text-violet-500" />
                        {t('pages.deviceAssistant.title')}
                    </h1>
                    <p className="text-muted-foreground">{t('pages.deviceAssistant.subtitle')}</p>
                </div>
            </div>
            {searchParams.has('rehearsal') ? <DeviceAssistantRehearsalGate
                rehearsalId={searchParams.get('rehearsal') ?? ''}
                deviceId={String(connection.device_id ?? connection.version_info.client_id ?? '')}
            >
                {row => <DeviceAssistantWorkspace
                    key={row.rehearsal_id}
                    rehearsal={row}
                    deskId={deskId}
                    stableDeviceId={connection.version_info.client_id ?? connection.device_id ?? deskId}
                    localPairingAvailable={!connection.device_id}
                    featureProfile={featureProfile}
                    assistantEnabled={isDeviceAssistantEnabled(connection.version_info)}
                    onBrowserTakeover={() => navigate(`/desk/${deskId}/control`)}
                />}
            </DeviceAssistantRehearsalGate> : (
                <DeviceAssistantWorkspace
                    deskId={deskId}
                    stableDeviceId={connection.version_info.client_id ?? connection.device_id ?? deskId}
                    localPairingAvailable={!connection.device_id}
                    featureProfile={featureProfile}
                    assistantEnabled={isDeviceAssistantEnabled(connection.version_info)}
                    onBrowserTakeover={() => navigate(`/desk/${deskId}/control`)}
                />
            )}
        </div>
    );
}
