import { AiAssistantIcon } from '@/components/ai-assistant-icon';
import { AssistantContextMeter } from './assistant-context-meter';
import { AssistantContextNotices, noticeMessageId } from './assistant-context-notices';
import { AssistantPermissionDisclosure } from './assistant-permission-disclosure';
import { AssistantPermissionRecords } from './assistant-permission-records';
import { AssistantHistory } from './assistant-history';
import { capabilityDescriptionKey } from './assistant-capability-copy';
import { CommandConfirmationCard, validCommandReview } from './device-assistant-command';
import { Fragment, type FormEvent, useEffect, useRef, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { AlertTriangle, ArrowLeft, Check, Copy, Eye, LoaderCircle, Monitor, Puzzle, RefreshCw, Send, ShieldCheck, Sparkles, X } from 'lucide-react';

import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Checkbox } from '@/components/ui/checkbox';
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
    ownerSelectableWindows,
    useDeviceAssistantObservation,
} from './use-device-assistant-observation';
import { useDeviceAssistantChat } from './use-device-assistant-chat';
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

function formatByteCount(value: number) {
    if (value < 1024) return `${value} B`;
    if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KiB`;
    return `${(value / (1024 * 1024)).toFixed(1)} MiB`;
}

type PermissionItemEdit = {
    resourceScope?: string[];
    operationScope?: string[];
    exportDestinationIndexes?: number[];
    ttlSeconds?: number;
    maxUses?: number;
};

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

function DeviceAssistantWorkspace({
    deskId,
    stableDeviceId,
    localPairingAvailable,
    featureProfile,
    assistantEnabled,
    onBrowserTakeover,
}: {
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
    const { entries, inspectSession, inspectUi, scheduleUi, cancelDelayedUi, remainingSeconds } = useDeviceAssistantObservation({
        deskId,
        enabled: assistantEnabled && isConnected,
        subscribe,
        sendMessage,
    });
    const chat = useDeviceAssistantChat({
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
    const [question, setQuestion] = useState('');
    const [panel, setPanel] = useState<AssistantPanelId | null>(null);
    const [selectedCapabilityIds, setSelectedCapabilityIds] = useState<string[]>([]);
    const [permissionSelections, setPermissionSelections] = useState<Record<string, string[]>>({});
    const [permissionEdits, setPermissionEdits] = useState<
        Record<string, Record<string, PermissionItemEdit>>
    >({});
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
        if (!assistantEnabled) return;
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
        setPermissionSelections({});
        setPermissionEdits({});
    };

    const updatePermissionItemEdit = (
        requestId: string,
        itemId: string,
        update: (current: PermissionItemEdit) => PermissionItemEdit,
    ) => {
        setPermissionEdits((current) => ({
            ...current,
            [requestId]: {
                ...current[requestId],
                [itemId]: update(current[requestId]?.[itemId] ?? {}),
            },
        }));
    };

    const togglePermissionScope = (
        requestId: string,
        itemId: string,
        field: 'resourceScope' | 'operationScope',
        value: string,
        defaults: string[],
    ) => {
        updatePermissionItemEdit(requestId, itemId, (current) => {
            const values = current[field] ?? defaults;
            return {
                ...current,
                [field]: values.includes(value)
                    ? values.filter((entry) => entry !== value)
                    : [...values, value],
            };
        });
    };

    const togglePermissionItem = (
        requestId: string,
        defaultItemIds: string[],
        itemId: string,
    ) => {
        setPermissionSelections((current) => {
            const selected = current[requestId] ?? defaultItemIds;
            return {
                ...current,
                [requestId]: selected.includes(itemId)
                    ? selected.filter((id) => id !== itemId)
                    : [...selected, itemId],
            };
        });
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
            <div className="flex flex-wrap items-center justify-between gap-3">
                <div className="min-w-0 text-sm text-muted-foreground">
                    <span>{chat.sessionTarget?.display_name ?? t('pages.deviceAssistant.title')}</span>
                </div>
                <Button type="button" variant="outline" size="sm" onClick={() => setPanel('details')}>
                    {t('pages.deviceAssistant.workspace.details')}
                </Button>
            </div>
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
            <Card className="mx-auto w-full max-w-4xl border-0 shadow-none">
                <CardHeader className="px-0">
                    <div className="flex flex-wrap items-start justify-between gap-3">
                        <div>
                            <CardTitle className="flex items-center gap-2 text-base">
                                <Sparkles className="h-4 w-4" />
                                {t('pages.deviceAssistant.chatTitle')}
                            </CardTitle>
                            <CardDescription>
                                {t('pages.deviceAssistant.providerBoundary', {
                                    provider: providerConfig?.wire_protocol ?? t('pages.deviceAssistant.providerUnknown'),
                                    model: providerConfig?.model ?? t('pages.deviceAssistant.providerUnknown'),
                                })}
                            </CardDescription>
                        </div>
                        <div className="flex flex-wrap items-center gap-2">
                            <Badge variant="outline">{t(`pages.deviceAssistant.chatPhase.${chat.status}`)}</Badge>
                            <AssistantHistory deskId={deskId} disabled={chat.running || chat.hydrating || !!chat.grantRevoking}
                                onSelect={(id) => {
                                    if (!chat.selectConversation(id)) return false;
                                    setQuestion('');
                                    setSelectedCapabilityIds([]);
                                    setPermissionSelections({});
                                    setPermissionEdits({});
                                    return true;
                                }} />
                            <Button variant="ghost" size="sm" onClick={resetConversation} disabled={!assistantEnabled || chat.running || chat.hydrating}>
                                {t('pages.deviceAssistant.newConversation')}
                            </Button>
                        </div>
                    </div>
                    <div role="status" data-testid="assistant-signal-status" className="flex items-center gap-2 text-sm text-muted-foreground">
                        <span className={`h-2 w-2 rounded-full ${isConnected ? 'bg-green-500' : 'bg-amber-500'}`} />
                        {t(isConnected ? 'pages.deviceAssistant.signalConnected' : 'pages.deviceAssistant.signalConnecting')}
                    </div>
                </CardHeader>
                <CardContent className="space-y-4 px-0">
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
                        {chat.messages.map((message) => (
                            <Fragment key={message.id}>
                            <div
                                key={message.id}
                                className={`max-w-[90%] rounded-lg px-3 py-2 text-sm ${
                                    message.role === 'user'
                                        ? 'ml-auto bg-muted'
                                        : message.role === 'tool_result' ? 'w-full border bg-muted/30' : 'w-full bg-transparent'
                                }`}
                            >
                                {message.role === 'tool_result' && <p className="mb-1 font-medium">{t('pages.deviceAssistant.commandResultTitle')}</p>}
                                {message.role === 'assistant'
                                    ? <MarkdownContent disableLinks>{message.text}</MarkdownContent>
                                    : <p className={message.role === 'tool_result' ? 'max-h-64 overflow-auto whitespace-pre-wrap break-words' : 'whitespace-pre-wrap'}>{message.text}</p>}
                                {message.role === 'tool_result' && <p className="mt-2 text-xs text-muted-foreground">{t('pages.deviceAssistant.commandResultHint')}</p>}
                            </div>
                            <AssistantContextNotices notices={chat.contextNotices.filter(notice => noticeMessageId(notice, chat.messages) === message.id)} />
                            </Fragment>
                        ))}
                        <AssistantContextNotices historical notices={chat.contextNotices.filter(notice => !noticeMessageId(notice, chat.messages))} />
                        {chat.partial && (
                            <MarkdownContent disableLinks className="max-w-[90%] rounded-lg bg-muted px-3 py-2 text-sm">
                                {chat.partial}
                            </MarkdownContent>
                        )}
                        {chat.running && !chat.partial && (
                            <div className="flex items-center gap-2 text-sm text-muted-foreground">
                                <LoaderCircle className="h-4 w-4 animate-spin" />
                                {t('pages.deviceAssistant.working')}
                            </div>
                        )}
                    </div>

                    {chat.unresolvedOutcome && (
                        <div data-testid="device-assistant-outcome-unknown" className="space-y-3 rounded-md border border-amber-500/50 bg-amber-500/5 p-3">
                            <div>
                                <p className="text-sm font-medium">{t('pages.deviceAssistant.outcomeUnknownTitle')}</p>
                                <p className="text-xs text-muted-foreground">
                                    {t('pages.deviceAssistant.outcomeUnknownDescription')}
                                </p>
                            </div>
                            <p className="break-all text-xs text-muted-foreground">
                                {chat.unresolvedOutcome.workKind} · work {chat.unresolvedOutcome.workId} · {chat.unresolvedOutcome.executionId}
                            </p>
                            {featureProfile.unknown_outcome_disposition && (
                            <Button
                                variant="outline"
                                size="sm"
                                disabled={chat.outcomeDisposing}
                                onClick={() => void chat.disposeUnknownOutcome()}
                            >
                                {chat.outcomeDisposing && <LoaderCircle className="mr-2 h-4 w-4 animate-spin" />}
                                {t('pages.deviceAssistant.outcomeUnknownDispose')}
                            </Button>
                            )}
                        </div>
                    )}
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
                    {chat.backgroundTasks.length > 0 && (
                        <div data-testid="device-assistant-background-tasks" className="space-y-3 rounded-md border border-blue-500/40 p-3">
                            <div>
                                <p className="text-sm font-medium">{t('pages.deviceAssistant.backgroundTitle')}</p>
                                <p className="text-xs text-muted-foreground">
                                    {t('pages.deviceAssistant.backgroundDescription')}
                                </p>
                            </div>
                            {chat.backgroundTasks.map((task) => (
                                <div key={task.taskId} className="space-y-2 rounded-md bg-muted/50 p-3">
                                    <div className="flex flex-wrap items-center justify-between gap-2">
                                        <div>
                                            <p className="text-sm font-medium">{task.toolName}</p>
                                            <p className="break-all text-xs text-muted-foreground">
                                                {task.providerId} · {task.capabilityId}
                                            </p>
                                        </div>
                                        <Badge variant={task.state === 'running' || task.state === 'cancel_requested' ? 'default' : 'outline'}>
                                            {t(`pages.deviceAssistant.backgroundState.${task.state}`)}
                                        </Badge>
                                    </div>
                                    <div className="flex flex-wrap gap-x-4 gap-y-1 text-xs text-muted-foreground">
                                        <span>{t('pages.deviceAssistant.backgroundProgress', { sequence: task.progressSequence })}</span>
                                        <span>{t('pages.deviceAssistant.backgroundUpdated', { time: task.updatedAt })}</span>
                                    </div>
                                </div>
                            ))}
                        </div>
                    )}
                    <AssistantPermissionRecords key={`${deskId}:${chat.conversationId}`} requests={chat.permissionRequests}>
                            {(request) => (
                                <AssistantPermissionDisclosure key={request.requestId} state={request.state} tools={request.items.map((item) => item.toolName)}>
                                    <div className="flex flex-wrap items-center justify-between gap-2">
                                        <span className="text-xs text-muted-foreground">
                                            rev {request.inputRevision}
                                        </span>
                                        <Badge variant={request.state === 'pending' ? 'default' : 'outline'}>
                                            {t(`pages.deviceAssistant.permissionState.${request.state}`)}
                                        </Badge>
                                    </div>
                                    <div className="space-y-2">
                                        {request.items.map((item) => {
                                            const defaultItemIds = request.items
                                                .filter((entry) => (entry.expectedEffect !== 'send_external'
                                                    || Boolean(entry.externalSendConfirmation))
                                                    && (entry.toolName !== 'execute_confirmed_command' || validCommandReview(entry.commandConfirmation)))
                                                .map((entry) => entry.itemId);
                                            const selected = permissionSelections[request.requestId]
                                                ?? defaultItemIds;
                                            const approved = selected.includes(item.itemId);
                                            const isExternalSend = item.expectedEffect === 'send_external';
                                            const sendConfirmation = item.externalSendConfirmation;
                                            const commandConfirmation = item.commandConfirmation;
                                            const commandBlocked = item.toolName === 'execute_confirmed_command' && !validCommandReview(commandConfirmation);
                                            const approvalBlocked = (isExternalSend && !sendConfirmation) || commandBlocked;
                                            const edit = permissionEdits[request.requestId]?.[item.itemId]
                                                ?? {};
                                            const resourceScope = edit.resourceScope
                                                ?? item.resourceScope;
                                            const operationScope = edit.operationScope
                                                ?? item.operationScope;
                                            const exportDestinationIndexes = edit.exportDestinationIndexes
                                                ?? item.exportDestinations.map((_, index) => index);
                                            return (
                                                <div key={item.itemId} className="flex items-start gap-3 rounded border bg-background px-3 py-2">
                                                    {featureProfile.permission_decision
                                                        && request.state === 'pending' && (
                                                        <Checkbox
                                                            className="mt-0.5"
                                                            checked={approved}
                                                            disabled={approvalBlocked}
                                                            aria-label={t('pages.deviceAssistant.permissionItemToggle', { reason: item.reason })}
                                                            onCheckedChange={() => togglePermissionItem(
                                                                request.requestId,
                                                                defaultItemIds,
                                                                item.itemId,
                                                            )}
                                                        />
                                                    )}
                                                    <div>
                                                        <p className="text-sm font-medium">{item.reason}</p>
                                                        <p className="mt-1 break-all text-xs text-muted-foreground">
                                                            {item.providerId} · {item.toolName} · {item.expectedEffect}
                                                        </p>
                                                        {validCommandReview(commandConfirmation) && <CommandConfirmationCard value={commandConfirmation} />}
                                                        {sendConfirmation && (
                                                            <div data-testid="external-send-confirmation" className="mt-3 space-y-2 rounded-md border border-red-500/50 bg-red-500/5 p-3 text-xs">
                                                                <p className="flex items-center gap-2 font-semibold text-red-700 dark:text-red-300">
                                                                    <AlertTriangle className="h-4 w-4" />
                                                                    {t('pages.deviceAssistant.externalSendConfirmationTitle')}
                                                                </p>
                                                                <p>{t('pages.deviceAssistant.externalSendOneShotWarning')}</p>
                                                                <dl className="grid gap-x-3 gap-y-1 sm:grid-cols-[max-content_1fr]">
                                                                    <dt className="font-medium">{t('pages.deviceAssistant.externalSendAccount')}</dt>
                                                                    <dd className="break-all">{sendConfirmation.accountId}</dd>
                                                                    <dt className="font-medium">{t('pages.deviceAssistant.externalSendDestination')}</dt>
                                                                    <dd className="break-all">{sendConfirmation.destination}</dd>
                                                                    {sendConfirmation.subject != null && (
                                                                        <>
                                                                            <dt className="font-medium">{t('pages.deviceAssistant.externalSendSubject')}</dt>
                                                                            <dd className="break-words">{sendConfirmation.subject}</dd>
                                                                        </>
                                                                    )}
                                                                    <dt className="font-medium">{t('pages.deviceAssistant.externalSendBody')}</dt>
                                                                    <dd>{formatByteCount(sendConfirmation.bodySizeBytes)}</dd>
                                                                </dl>
                                                                <pre className="max-h-48 overflow-auto whitespace-pre-wrap break-words rounded bg-background p-2">
                                                                    {sendConfirmation.bodyPlainText}
                                                                </pre>
                                                                {sendConfirmation.attachments.length > 0 && (
                                                                    <div>
                                                                        <p className="font-medium">{t('pages.deviceAssistant.externalSendAttachments')}</p>
                                                                        <ul className="list-disc pl-5">
                                                                            {sendConfirmation.attachments.map((attachment) => (
                                                                                <li key={`${attachment.fileName}:${attachment.sizeBytes}`} className="break-all">
                                                                                    {attachment.fileName} · {formatByteCount(attachment.sizeBytes)}
                                                                                </li>
                                                                            ))}
                                                                        </ul>
                                                                    </div>
                                                                )}
                                                            </div>
                                                        )}
                                                        {approvalBlocked && (
                                                            <p className="mt-2 text-xs font-medium text-red-700 dark:text-red-300">
                                                                {t(commandBlocked ? 'pages.deviceAssistant.commandSummaryMissing' : 'pages.deviceAssistant.externalSendSummaryMissing')}
                                                            </p>
                                                        )}
                                                        {(item.resourceScope.length > 0 || item.operationScope.length > 0) && (
                                                            <p className="mt-1 break-all text-xs text-muted-foreground">
                                                                {[...item.resourceScope, ...item.operationScope].join(' · ')}
                                                            </p>
                                                        )}
                                                        {featureProfile.permission_decision
                                                            && request.state === 'pending'
                                                            && approved && (
                                                            <div className="mt-3 space-y-3 border-t pt-3">
                                                                {item.resourceScope.length > 0 && (
                                                                    <div className="space-y-1">
                                                                        <p className="text-xs font-medium">
                                                                            {t('pages.deviceAssistant.permissionResourceScope')}
                                                                        </p>
                                                                        {item.resourceScope.map((scope) => (
                                                                            <label key={scope} className="flex items-center gap-2 text-xs">
                                                                                <Checkbox
                                                                                    checked={resourceScope.includes(scope)}
                                                                                    onCheckedChange={() => togglePermissionScope(
                                                                                        request.requestId,
                                                                                        item.itemId,
                                                                                        'resourceScope',
                                                                                        scope,
                                                                                        item.resourceScope,
                                                                                    )}
                                                                                />
                                                                                <span className="break-all">{scope}</span>
                                                                            </label>
                                                                        ))}
                                                                    </div>
                                                                )}
                                                                {item.operationScope.length > 0 && (
                                                                    <div className="space-y-1">
                                                                        <p className="text-xs font-medium">
                                                                            {t('pages.deviceAssistant.permissionOperationScope')}
                                                                        </p>
                                                                        {item.operationScope.map((scope) => (
                                                                            <label key={scope} className="flex items-center gap-2 text-xs">
                                                                                <Checkbox
                                                                                    checked={operationScope.includes(scope)}
                                                                                    onCheckedChange={() => togglePermissionScope(
                                                                                        request.requestId,
                                                                                        item.itemId,
                                                                                        'operationScope',
                                                                                        scope,
                                                                                        item.operationScope,
                                                                                    )}
                                                                                />
                                                                                <span className="break-all">{scope}</span>
                                                                            </label>
                                                                        ))}
                                                                    </div>
                                                                )}
                                                                {item.exportDestinations.length > 0 && (
                                                                    <div className="space-y-1">
                                                                        <p className="text-xs font-medium">
                                                                            {t('pages.deviceAssistant.permissionDestinations')}
                                                                        </p>
                                                                        {item.exportDestinations.map((destination, index) => (
                                                                            <label key={JSON.stringify(destination)} className="flex items-center gap-2 text-xs">
                                                                                <Checkbox
                                                                                    checked={exportDestinationIndexes.includes(index)}
                                                                                    onCheckedChange={() => updatePermissionItemEdit(
                                                                                        request.requestId,
                                                                                        item.itemId,
                                                                                        (current) => {
                                                                                            const indexes = current.exportDestinationIndexes
                                                                                                ?? item.exportDestinations.map((_, currentIndex) => currentIndex);
                                                                                            return {
                                                                                                ...current,
                                                                                                exportDestinationIndexes: indexes.includes(index)
                                                                                                    ? indexes.filter((entry) => entry !== index)
                                                                                                    : [...indexes, index],
                                                                                            };
                                                                                        },
                                                                                    )}
                                                                                />
                                                                                <span className="break-all">{JSON.stringify(destination)}</span>
                                                                            </label>
                                                                        ))}
                                                                    </div>
                                                                )}
                                                                <div className="grid gap-3 sm:grid-cols-2">
                                                                    <label className="space-y-1 text-xs">
                                                                        <span>{t('pages.deviceAssistant.permissionTtlSeconds')}</span>
                                                                        <Input
                                                                            type="number"
                                                                            min={1}
                                                                            max={item.suggestedTtlSeconds}
                                                                            value={edit.ttlSeconds ?? item.suggestedTtlSeconds}
                                                                            onChange={(event) => updatePermissionItemEdit(
                                                                                request.requestId,
                                                                                item.itemId,
                                                                                (current) => ({
                                                                                    ...current,
                                                                                    ttlSeconds: Math.max(1, Math.min(
                                                                                        item.suggestedTtlSeconds,
                                                                                        Number(event.target.value) || 1,
                                                                                    )),
                                                                                }),
                                                                            )}
                                                                        />
                                                                    </label>
                                                                    <label className="space-y-1 text-xs">
                                                                        <span>{t('pages.deviceAssistant.permissionMaxUses')}</span>
                                                                        <Input
                                                                            type="number"
                                                                            min={1}
                                                                            max={item.suggestedMaxUses}
                                                                            value={isExternalSend ? 1 : (edit.maxUses ?? item.suggestedMaxUses)}
                                                                            disabled={isExternalSend}
                                                                            onChange={(event) => updatePermissionItemEdit(
                                                                                request.requestId,
                                                                                item.itemId,
                                                                                (current) => ({
                                                                                    ...current,
                                                                                    maxUses: Math.max(1, Math.min(
                                                                                        item.suggestedMaxUses,
                                                                                        Number(event.target.value) || 1,
                                                                                    )),
                                                                                }),
                                                                            )}
                                                                        />
                                                                    </label>
                                                                </div>
                                                            </div>
                                                        )}
                                                    </div>
                                                </div>
                                            );
                                        })}
                                    </div>
                                    {featureProfile.permission_decision
                                        && request.state === 'pending' && (
                                        <div className="space-y-2">
                                            <p className="text-xs text-muted-foreground">
                                                {t('pages.deviceAssistant.permissionSelectionDescription')}
                                            </p>
                                            <div className="flex flex-wrap gap-2">
                                            <Button
                                                type="button"
                                                size="sm"
                                                disabled={!assistantEnabled || chat.permissionUpdating}
                                                onClick={() => void chat.decidePermissionItems(
                                                    request,
                                                    request.items.map((item) => {
                                                        const selected = permissionSelections[request.requestId]
                                                            ?? request.items
                                                                .filter((entry) => (entry.expectedEffect !== 'send_external'
                                                                    || Boolean(entry.externalSendConfirmation))
                                                                    && (entry.toolName !== 'execute_confirmed_command' || validCommandReview(entry.commandConfirmation)))
                                                                .map((entry) => entry.itemId);
                                                        if (!selected.includes(item.itemId)
                                                            || (item.expectedEffect === 'send_external'
                                                                && !item.externalSendConfirmation)
                                                            || (item.toolName === 'execute_confirmed_command' && !validCommandReview(item.commandConfirmation))) {
                                                            return {
                                                                itemId: item.itemId,
                                                                decision: 'deny' as const,
                                                            };
                                                        }
                                                        const edit = permissionEdits[request.requestId]?.[item.itemId]
                                                            ?? {};
                                                        const destinationIndexes = edit.exportDestinationIndexes
                                                            ?? item.exportDestinations.map((_, index) => index);
                                                        return {
                                                            itemId: item.itemId,
                                                            decision: 'approve' as const,
                                                            resource_scope: edit.resourceScope ?? item.resourceScope,
                                                            operation_scope: edit.operationScope ?? item.operationScope,
                                                            export_destinations: item.exportDestinations.filter((_, index) =>
                                                                destinationIndexes.includes(index)),
                                                            ttl_seconds: edit.ttlSeconds ?? item.suggestedTtlSeconds,
                                                            max_uses: item.expectedEffect === 'send_external'
                                                                ? 1
                                                                : (edit.maxUses ?? item.suggestedMaxUses),
                                                        };
                                                    }),
                                                )}
                                            >
                                                <Check className="mr-2 h-4 w-4" />
                                                {t('pages.deviceAssistant.permissionSubmitSelection')}
                                            </Button>
                                            <Button
                                                type="button"
                                                size="sm"
                                                variant="outline"
                                                disabled={chat.permissionUpdating}
                                                onClick={() => void chat.decidePermission(request, false)}
                                            >
                                                <X className="mr-2 h-4 w-4" />
                                                {t('pages.deviceAssistant.permissionDeny')}
                                            </Button>
                                            </div>
                                        </div>
                                    )}
                                    {request.state === 'needs_revalidation' && (
                                        <p className="text-xs text-amber-700 dark:text-amber-300">
                                            {t('pages.deviceAssistant.permissionNeedsRevalidation')}
                                        </p>
                                    )}
                                </AssistantPermissionDisclosure>
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
                    {chat.visualEvidence.length > 0 && (
                        <div data-testid="device-assistant-visual-evidence" className="grid gap-3 sm:grid-cols-2">
                            {chat.visualEvidence.map((evidence) => (
                                <div key={evidence.evidence_id} className="overflow-hidden rounded-md border bg-muted/30">
                                    {evidence.preview_data_url ? (
                                        <img
                                            src={evidence.preview_data_url}
                                            alt={t('pages.deviceAssistant.visualEvidenceAlt')}
                                            className="max-h-56 w-full object-contain"
                                        />
                                    ) : (
                                        <div className="flex h-24 items-center justify-center px-3 text-center text-xs text-muted-foreground">
                                            {evidence.status === 'expired'
                                                ? t('pages.deviceAssistant.visualEvidenceExpired')
                                                : t('pages.deviceAssistant.visualEvidenceNotRetained')}
                                        </div>
                                    )}
                                    <div className="space-y-1 border-t p-2 text-xs">
                                        <div>{t(`pages.deviceAssistant.visualEvidencePhase.${evidence.phase}`)}</div>
                                        <div className="text-muted-foreground">
                                            {new Date(evidence.captured_at_unix_ms).toLocaleString()}
                                        </div>
                                    </div>
                                </div>
                            ))}
                        </div>
                    )}
                    {chat.error && (
                        <Alert variant="destructive">
                            <AlertTitle>{t('pages.deviceAssistant.chatErrorTitle')}</AlertTitle>
                            <AlertDescription>{chat.error === 'history_restore_failed' ? t('pages.deviceAssistant.history.restoreError') : chat.error}</AlertDescription>
                        </Alert>
                    )}
                    <form onSubmit={submit} className="sticky bottom-0 space-y-2 rounded-xl border bg-background p-3 shadow-sm">
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
                            onChange={(event) => setQuestion(event.target.value)}
                            placeholder={t('pages.deviceAssistant.questionPlaceholder')}
                            maxLength={16_384}
                            disabled={!assistantEnabled || !isConnected || chat.hydrating || chat.contextUpdating || !providerConfig?.api_key_set || !providerConfig?.model}
                            className="min-h-16 w-full resize-y rounded-md border-0 bg-background px-3 py-2 text-sm shadow-sm outline-none placeholder:text-muted-foreground focus-visible:ring-1 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50"
                        />
                        <div className="flex items-center justify-between gap-3">
                            <AssistantContextMeter usage={chat.contextUsage} draft={question} />
                            <Button type="submit" disabled={!assistantEnabled || !question.trim() || !isConnected || chat.hydrating || !chat.sessionTargetReady || chat.sessionTargetResolving || chat.contextUpdating || !providerConfig?.api_key_set || !providerConfig?.model}>
                                <Send className="mr-2 h-4 w-4" />
                                {t('pages.deviceAssistant.send')}
                            </Button>
                        </div>
                    </form>
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
        <div className="mx-auto max-w-6xl space-y-6 p-6">
            <div className="flex items-center gap-4">
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
            <DeviceAssistantWorkspace
                deskId={deskId}
                stableDeviceId={connection.version_info.client_id ?? connection.device_id ?? deskId}
                localPairingAvailable={!connection.device_id}
                featureProfile={featureProfile}
                assistantEnabled={isDeviceAssistantEnabled(connection.version_info)}
                onBrowserTakeover={() => navigate(`/desk/${deskId}/control`)}
            />
        </div>
    );
}
