import { permissionToolLabel, permissionEffectLabel, permissionResourceLabel, permissionOperationLabel } from './assistant-permission-labels';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { AlertTriangle, Check, X } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import { Input } from '@/components/ui/input';
import type { GrantRequestItemDto, PermissionDecisionBody, PermissionRequestDto } from '@/services/types';
import { OutputConfirmationCard, validOutputReview, isOutputInput, outputApprovalBlocked } from './ai-assistant-output-confirmation';
import { AssistantPermissionDisclosure } from './assistant-permission-disclosure';
import { CommandConfirmationCard, validCommandReview } from './ai-assistant-command';
import { LaunchConfirmationCard, validLaunchReview } from './ai-assistant-launch';
import { TextFileConfirmationCard, validTextFileReview, fileApprovalBlocked } from './ai-assistant-file-confirmation';

function needsApplicationScope(tool: string) {
    return ['execute_ui_actions', 'send_background_input'].includes(tool);
}

function hasNativeUiAuthority(item: GrantRequestItemDto) {
    return !needsApplicationScope(item.toolName)
        || Boolean(item.applicationScope);
}

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

type PermissionDecisionView = {
    source: 'owner' | 'ai_approval' | 'review_unavailable';
    items: { itemId: string; approved: boolean; reasonCode?: string | null; reason?: string | null }[];
};

export function AssistantPermissionRequest({ request, canDecide, disabled = false, busy = false, waitingForTurn = false, onDecide }: {
    request: PermissionRequestDto;
    canDecide: boolean;
    disabled?: boolean;
    busy?: boolean;
    waitingForTurn?: boolean;
    onDecide: (request: PermissionRequestDto, items: PermissionDecisionBody['items']) => Promise<boolean>;
}) {
    const { t } = useTranslation();
    const decision = (request as PermissionRequestDto & { decision?: PermissionDecisionView | null }).decision;
    const [permissionSelections, setPermissionSelections] = useState<Record<string, string[]>>({});
    const [permissionEdits, setPermissionEdits] = useState<
        Record<string, Record<string, PermissionItemEdit>>
    >({});
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


    return <AssistantPermissionDisclosure state={request.state} tools={request.items.map((item) => item.toolName)}>
        <fieldset disabled={disabled || busy || waitingForTurn} className="min-w-0 space-y-3">
            {waitingForTurn && request.state === 'pending' && <p role="status" className="text-xs text-muted-foreground">
                {t('pages.aiAssistant.permissionWaitingForTurn')}
            </p>}
            {decision && <p className="text-xs text-muted-foreground">
                {t(`pages.aiAssistant.permissionDecisionSource.${decision.source}`)}
            </p>}
            {request.items.some((item) => ['execute_ui_actions', 'send_background_input', 'send_raw_input'].includes(item.toolName))
                && ['inspect_desktop_session', 'inspect_desktop_ui'].every((name) => request.items.some((item) => item.toolName === name)) && (
                <p className="text-xs text-muted-foreground">{t('pages.aiAssistant.permissionIncludedDesktopReads')}</p>
            )}
            <div className="space-y-2">
                {request.items.map((item) => {
                    const decidedItem = decision?.items.find((entry) => entry.itemId === item.itemId);
                    const reason = item.itemId === `included-${item.toolName}`
                        && ['inspect_desktop_session', 'inspect_desktop_ui'].includes(item.toolName)
                        ? t('pages.aiAssistant.permissionIncludedDesktopReadReason') : item.reason;
                    const defaultItemIds = request.items
                        .filter((entry) => (entry.expectedEffect !== 'send_external'
                            || Boolean(entry.externalSendConfirmation))
                            && (entry.toolName !== 'exec_command' || validCommandReview(entry.commandConfirmation))
                            && (entry.toolName !== 'launch_application' || validLaunchReview(entry.launchConfirmation))
                            && hasNativeUiAuthority(entry)
                            && !fileApprovalBlocked(entry) && !outputApprovalBlocked(entry))
                        .map((entry) => entry.itemId);
                    const selected = permissionSelections[request.requestId]
                        ?? defaultItemIds;
                    const approved = selected.includes(item.itemId);
                    const isExternalSend = item.expectedEffect === 'send_external';
                    const sendConfirmation = item.externalSendConfirmation;
                    const commandConfirmation = item.commandConfirmation;
                    const outputBlocked = outputApprovalBlocked(item);
                    const commandBlocked = item.toolName === 'exec_command' && !validCommandReview(commandConfirmation);
                    const launchBlocked = item.toolName === 'launch_application' && !validLaunchReview(item.launchConfirmation);
                    const approvalBlocked = outputBlocked || (isExternalSend && !sendConfirmation) || commandBlocked || launchBlocked || !hasNativeUiAuthority(item) || fileApprovalBlocked(item);
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
                            {canDecide
                                && request.state === 'pending' && (
                                <Checkbox
                                    className="mt-0.5"
                                    checked={approved}
                                    disabled={approvalBlocked}
                                    aria-label={t('pages.aiAssistant.permissionItemToggle', { reason })}
                                    onCheckedChange={() => togglePermissionItem(
                                        request.requestId,
                                        defaultItemIds,
                                        item.itemId,
                                    )}
                                />
                            )}
                            <div>
                                <p className="text-sm font-medium">{reason}</p>
                                {decidedItem && <div className="mt-2 space-y-1 text-xs">
                                    <p className={decidedItem.approved ? 'text-emerald-700 dark:text-emerald-300' : 'text-red-700 dark:text-red-300'}>
                                        {t(`pages.aiAssistant.permissionItemDecision.${decidedItem.approved ? 'approved' : 'denied'}`)}
                                    </p>
                                    {decidedItem.reason && <p className="whitespace-pre-wrap break-words text-muted-foreground">
                                        {t('pages.aiAssistant.permissionAiReason')}: {decidedItem.reason}
                                    </p>}
                                </div>}
                                <p className="mt-1 break-all text-xs text-muted-foreground">
                                    {permissionToolLabel(t, item.toolName)} · {permissionEffectLabel(t, item.expectedEffect)}
                                </p>
                                {item.applicationScope && (
                                    <div data-testid="application-ui-scope" className="mt-3 space-y-1 rounded-md border p-3 text-xs">
                                        <p className="font-medium">{t('pages.aiAssistant.applicationUiScopeTitle', { name: item.applicationScope.application_name })}</p>
                                        <p>{t('pages.aiAssistant.applicationUiScopeDescription')}</p>
                                        <p>{t('pages.aiAssistant.applicationUiScopeLifetime')}</p>
                                        <p>{item.applicationScope.actions.map((action) => t(`pages.aiAssistant.uiAction_${action}`)).join(' · ')}</p>
                                    </div>
                                )}
                                {validOutputReview(item.waylandOutputConfirmation) && <OutputConfirmationCard value={item.waylandOutputConfirmation} />}
                                {validCommandReview(commandConfirmation) && <CommandConfirmationCard value={commandConfirmation} />}
                                {validLaunchReview(item.launchConfirmation) && <LaunchConfirmationCard value={item.launchConfirmation} />}
                                {validTextFileReview(item.textFileConfirmation) && <TextFileConfirmationCard value={item.textFileConfirmation} />}
                                {sendConfirmation && (
                                    <div data-testid="external-send-confirmation" className="mt-3 space-y-2 rounded-md border border-red-500/50 bg-red-500/5 p-3 text-xs">
                                        <p className="flex items-center gap-2 font-semibold text-red-700 dark:text-red-300">
                                            <AlertTriangle className="h-4 w-4" />
                                            {t('pages.aiAssistant.externalSendConfirmationTitle')}
                                        </p>
                                        <p>{t('pages.aiAssistant.externalSendOneShotWarning')}</p>
                                        <dl className="grid gap-x-3 gap-y-1 sm:grid-cols-[max-content_1fr]">
                                            <dt className="font-medium">{t('pages.aiAssistant.externalSendAccount')}</dt>
                                            <dd className="break-all">{sendConfirmation.accountId}</dd>
                                            <dt className="font-medium">{t('pages.aiAssistant.externalSendDestination')}</dt>
                                            <dd className="break-all">{sendConfirmation.destination}</dd>
                                            {sendConfirmation.subject != null && (
                                                <>
                                                    <dt className="font-medium">{t('pages.aiAssistant.externalSendSubject')}</dt>
                                                    <dd className="break-words">{sendConfirmation.subject}</dd>
                                                </>
                                            )}
                                            <dt className="font-medium">{t('pages.aiAssistant.externalSendBody')}</dt>
                                            <dd>{formatByteCount(sendConfirmation.bodySizeBytes)}</dd>
                                        </dl>
                                        <pre className="max-h-48 overflow-auto whitespace-pre-wrap break-words rounded bg-background p-2">
                                            {sendConfirmation.bodyPlainText}
                                        </pre>
                                        {sendConfirmation.attachments.length > 0 && (
                                            <div>
                                                <p className="font-medium">{t('pages.aiAssistant.externalSendAttachments')}</p>
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
                                        {t(outputBlocked ? 'pages.aiAssistant.outputConfirmMissing' : needsApplicationScope(item.toolName) && !hasNativeUiAuthority(item) ? 'pages.aiAssistant.applicationUiScopeMissing' : fileApprovalBlocked(item) ? 'pages.aiAssistant.fileConfirmMissing'
                                            : launchBlocked ? 'pages.aiAssistant.launchSummaryMissing' : commandBlocked ? 'pages.aiAssistant.commandSummaryMissing' : 'pages.aiAssistant.externalSendSummaryMissing')}
                                    </p>
                                )}
                                {!item.applicationScope && (item.resourceScope.length > 0 || item.operationScope.length > 0) && (
                                    <p className="mt-1 break-all text-xs text-muted-foreground">
                                        {[...item.resourceScope.map((scope) => permissionResourceLabel(t, scope)), ...item.operationScope.map((scope) => permissionOperationLabel(t, scope))].join(' · ')}
                                    </p>
                                )}
                                {canDecide
                                    && request.state === 'pending'
                                    && approved && (
                                    <div className="mt-3 space-y-3 border-t pt-3">
                                        {item.resourceScope.length > 0 && (
                                            <div className="space-y-1">
                                                <p className="text-xs font-medium">
                                                    {t('pages.aiAssistant.permissionResourceScope')}
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
                                                        <span className="break-all">{permissionResourceLabel(t, scope, item.applicationScope?.application_name)}</span>
                                                    </label>
                                                ))}
                                            </div>
                                        )}
                                        {item.applicationScope && <p className="text-xs text-muted-foreground">{t('pages.aiAssistant.batchGrantUses')}</p>}
                                        {item.operationScope.length > 0 && (
                                            <div className="space-y-1">
                                                <p className="text-xs font-medium">
                                                    {t('pages.aiAssistant.permissionOperationScope')}
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
                                                        <span className="break-all">{permissionOperationLabel(t, scope)}</span>
                                                    </label>
                                                ))}
                                            </div>
                                        )}
                                        {item.exportDestinations.length > 0 && (
                                            <div className="space-y-1">
                                                <p className="text-xs font-medium">
                                                    {t('pages.aiAssistant.permissionDestinations')}
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
                                                <span>{t('pages.aiAssistant.permissionTtlSeconds')}</span>
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
                                                <span>{t('pages.aiAssistant.permissionMaxUses')}</span>
                                                <Input
                                                    type="number"
                                                    min={1}
                                                    max={item.suggestedMaxUses}
                                                    value={isExternalSend || isOutputInput(item) ? 1 : (edit.maxUses ?? item.suggestedMaxUses)}
                                                    disabled={isExternalSend || isOutputInput(item)}
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
            {canDecide
                && request.state === 'pending' && (
                <div className="space-y-2">
                    <p className="text-xs text-muted-foreground">
                        {t('pages.aiAssistant.permissionSelectionDescription')}
                    </p>
                    <div className="flex flex-wrap gap-2">
                    <Button
                        type="button"
                        size="sm"
                        className="gap-1.5 px-2.5"
                        disabled={disabled || busy || waitingForTurn}
                        onClick={() => void onDecide(
                            request,
                            request.items.map((item) => {
                                const selected = permissionSelections[request.requestId]
                                    ?? request.items
                                        .filter((entry) => (entry.expectedEffect !== 'send_external'
                                            || Boolean(entry.externalSendConfirmation))
                                            && (entry.toolName !== 'exec_command' || validCommandReview(entry.commandConfirmation))
                            && (entry.toolName !== 'launch_application' || validLaunchReview(entry.launchConfirmation))
                                            && hasNativeUiAuthority(entry)
                                            && !fileApprovalBlocked(entry) && !outputApprovalBlocked(entry))
                                        .map((entry) => entry.itemId);
                                if (!selected.includes(item.itemId)
                                    || (item.expectedEffect === 'send_external'
                                        && !item.externalSendConfirmation)
                                    || (item.toolName === 'exec_command' && !validCommandReview(item.commandConfirmation))
                                    || (item.toolName === 'launch_application' && !validLaunchReview(item.launchConfirmation))
                                    || outputApprovalBlocked(item) || fileApprovalBlocked(item) || !hasNativeUiAuthority(item)) {
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
                                    max_uses: item.expectedEffect === 'send_external' || isOutputInput(item)
                                        ? 1
                                        : (edit.maxUses ?? item.suggestedMaxUses),
                                };
                            }),
                        )}
                    >
                        <Check className="h-4 w-4" />
                        {t('pages.aiAssistant.permissionSubmitSelection')}
                    </Button>
                    <Button
                        type="button"
                        size="sm"
                        variant="outline"
                        className="gap-1.5 px-2.5"
                        disabled={disabled || busy || waitingForTurn}
                        onClick={() => void onDecide(request, request.items.map(item => ({ itemId: item.itemId, decision: 'deny' })))}
                    >
                        <X className="h-4 w-4" />
                        {t('pages.aiAssistant.permissionDeny')}
                    </Button>
                    </div>
                </div>
            )}
            {request.state === 'needs_revalidation' && (
                <p className="text-xs text-amber-700 dark:text-amber-300">
                    {t('pages.aiAssistant.permissionNeedsRevalidation')}
                </p>
            )}
        </fieldset>
    </AssistantPermissionDisclosure>;
}
