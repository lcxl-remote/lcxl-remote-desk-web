import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { AlertTriangle, Check, X } from 'lucide-react';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import { Input } from '@/components/ui/input';
import type { PermissionDecisionBody, PermissionRequestDto } from '@/services/types';
import { AssistantPermissionDisclosure } from './assistant-permission-disclosure';
import { CommandConfirmationCard, validCommandReview } from './device-assistant-command';
import { TextFileConfirmationCard, validTextFileReview, fileApprovalBlocked } from './device-assistant-file-confirmation';

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

export function AssistantPermissionRequest({ request, canDecide, disabled = false, busy = false, onDecide }: {
    request: PermissionRequestDto;
    canDecide: boolean;
    disabled?: boolean;
    busy?: boolean;
    onDecide: (request: PermissionRequestDto, items: PermissionDecisionBody['items']) => Promise<boolean>;
}) {
    const { t } = useTranslation();
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
        <fieldset disabled={disabled || busy} className="min-w-0 space-y-3">
            <div className="flex flex-wrap items-center justify-between gap-2">
                <span className="text-xs text-muted-foreground">
                    rev {request.inputRevision}
                </span>
                <Badge variant={request.state === 'pending' ? 'default' : 'outline'}>
                    {t(`pages.deviceAssistant.permissionState.${request.state}`)}
                </Badge>
            </div>
            {request.items.some((item) => ['execute_confirmed_ui_action', 'execute_confirmed_raw_input'].includes(item.toolName))
                && ['inspect_desktop_session', 'inspect_desktop_ui'].every((name) => request.items.some((item) => item.toolName === name)) && (
                <p className="text-xs text-muted-foreground">{t('pages.deviceAssistant.permissionIncludedDesktopReads')}</p>
            )}
            <div className="space-y-2">
                {request.items.map((item) => {
                    const reason = item.itemId === `included-${item.toolName}`
                        && ['inspect_desktop_session', 'inspect_desktop_ui'].includes(item.toolName)
                        ? t('pages.deviceAssistant.permissionIncludedDesktopReadReason') : item.reason;
                    const defaultItemIds = request.items
                        .filter((entry) => (entry.expectedEffect !== 'send_external'
                            || Boolean(entry.externalSendConfirmation))
                            && (entry.toolName !== 'execute_confirmed_command' || validCommandReview(entry.commandConfirmation))
                            && !fileApprovalBlocked(entry))
                        .map((entry) => entry.itemId);
                    const selected = permissionSelections[request.requestId]
                        ?? defaultItemIds;
                    const approved = selected.includes(item.itemId);
                    const isExternalSend = item.expectedEffect === 'send_external';
                    const sendConfirmation = item.externalSendConfirmation;
                    const commandConfirmation = item.commandConfirmation;
                    const commandBlocked = item.toolName === 'execute_confirmed_command' && !validCommandReview(commandConfirmation);
                    const approvalBlocked = (isExternalSend && !sendConfirmation) || commandBlocked || fileApprovalBlocked(item);
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
                                    aria-label={t('pages.deviceAssistant.permissionItemToggle', { reason })}
                                    onCheckedChange={() => togglePermissionItem(
                                        request.requestId,
                                        defaultItemIds,
                                        item.itemId,
                                    )}
                                />
                            )}
                            <div>
                                <p className="text-sm font-medium">{reason}</p>
                                <p className="mt-1 break-all text-xs text-muted-foreground">
                                    {item.providerId} · {item.toolName} · {item.expectedEffect}
                                </p>
                                {validCommandReview(commandConfirmation) && <CommandConfirmationCard value={commandConfirmation} />}
                                {validTextFileReview(item.textFileConfirmation) && <TextFileConfirmationCard value={item.textFileConfirmation} />}
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
                                        {t(fileApprovalBlocked(item) ? 'pages.deviceAssistant.fileConfirmMissing'
                                            : commandBlocked ? 'pages.deviceAssistant.commandSummaryMissing' : 'pages.deviceAssistant.externalSendSummaryMissing')}
                                    </p>
                                )}
                                {(item.resourceScope.length > 0 || item.operationScope.length > 0) && (
                                    <p className="mt-1 break-all text-xs text-muted-foreground">
                                        {[...item.resourceScope, ...item.operationScope].join(' · ')}
                                    </p>
                                )}
                                {canDecide
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
            {canDecide
                && request.state === 'pending' && (
                <div className="space-y-2">
                    <p className="text-xs text-muted-foreground">
                        {t('pages.deviceAssistant.permissionSelectionDescription')}
                    </p>
                    <div className="flex flex-wrap gap-2">
                    <Button
                        type="button"
                        size="sm"
                        disabled={disabled || busy}
                        onClick={() => void onDecide(
                            request,
                            request.items.map((item) => {
                                const selected = permissionSelections[request.requestId]
                                    ?? request.items
                                        .filter((entry) => (entry.expectedEffect !== 'send_external'
                                            || Boolean(entry.externalSendConfirmation))
                                            && (entry.toolName !== 'execute_confirmed_command' || validCommandReview(entry.commandConfirmation))
                                            && !fileApprovalBlocked(entry))
                                        .map((entry) => entry.itemId);
                                if (!selected.includes(item.itemId)
                                    || (item.expectedEffect === 'send_external'
                                        && !item.externalSendConfirmation)
                                    || (item.toolName === 'execute_confirmed_command' && !validCommandReview(item.commandConfirmation))
                                    || fileApprovalBlocked(item)) {
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
                        disabled={disabled || busy}
                        onClick={() => void onDecide(request, request.items.map(item => ({ itemId: item.itemId, decision: 'deny' })))}
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
        </fieldset>
    </AssistantPermissionDisclosure>;
}
