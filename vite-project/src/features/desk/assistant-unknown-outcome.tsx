import type { ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { AlertTriangle } from 'lucide-react';
import type { DeviceAssistantUnknownOutcome } from './use-device-assistant-chat';

export function AssistantUnknownOutcome({ outcome, children }: {
    outcome: DeviceAssistantUnknownOutcome;
    children?: ReactNode;
}) {
    const { t } = useTranslation();
    const kind = outcome.workKind === 'computer_action' ? 'computer'
        : outcome.workKind === 'agent_exec' ? 'command' : 'other';
    return <div data-testid="device-assistant-outcome-unknown" className="space-y-3 rounded-md border border-amber-500/50 bg-amber-500/5 p-3">
        <p className="flex items-center gap-2 text-sm font-medium">
            <AlertTriangle className="h-4 w-4 shrink-0" aria-hidden="true" />
            {t(`pages.deviceAssistant.unknownOutcome.title.${kind}`)}
        </p>
        {outcome.permissionReason && <p className="text-sm font-medium">{t('pages.deviceAssistant.permissionReasonLabel', { reason: outcome.permissionReason })}</p>}
        <p className="text-sm">{t('pages.deviceAssistant.unknownOutcome.reason')}</p>
        <p className="text-sm text-muted-foreground">{t(`pages.deviceAssistant.unknownOutcome.check.${kind}`)}</p>
        <p className="text-xs text-muted-foreground">{t('pages.deviceAssistant.unknownOutcome.receiptHint')}</p>
        {children}
        <details className="text-xs text-muted-foreground">
            <summary className="cursor-pointer py-1">{t('pages.deviceAssistant.unknownOutcome.technicalDetails')}</summary>
            <dl className="mt-2 space-y-2 [overflow-wrap:anywhere]">
                <div><dt>{t('pages.deviceAssistant.unknownOutcome.workId')}</dt><dd>{outcome.workId}</dd></div>
                <div><dt>{t('pages.deviceAssistant.unknownOutcome.requestId')}</dt><dd>{outcome.actionRequestId}</dd></div>
                <div><dt>{t('pages.deviceAssistant.unknownOutcome.executionId')}</dt><dd>{outcome.executionId}</dd></div>
                <div><dt>{t('pages.deviceAssistant.unknownOutcome.workKind')}</dt><dd>{outcome.workKind}</dd></div>
            </dl>
        </details>
    </div>;
}
