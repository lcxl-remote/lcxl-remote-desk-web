import { AssistantCodeBlock } from './assistant-code-block';
import { Disclosure } from '@/components/ui/disclosure';
import { useState } from 'react';
import { CheckCircle2, CircleHelp, Loader2, XCircle } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import type { AiAssistantToolActivity } from './use-ai-assistant-chat';
import { hasUnknownActionResult } from './action-result-status';

export function isHistoricalPermissionSkip(output: string | null | undefined): boolean {
    return output?.startsWith('not executed: waiting for user permission decision')
        || output?.startsWith('not executed: waiting for the existing user permission decision')
        || false;
}

function isPermissionSubmissionReceipt(tool: AiAssistantToolActivity): boolean {
    if (tool.name !== 'request_permissions' || !tool.output) return false;
    try {
        const result = JSON.parse(tool.output);
        return result?.status === 'pending_user_decision'
            && typeof result.request_id === 'string';
    } catch { return false; }
}

function batchResult(tool: AiAssistantToolActivity) {
    if (!['execute_ui_actions', 'send_background_input'].includes(tool.name) || !tool.output) return null;
    try {
        let value = JSON.parse(tool.output);
        if (typeof value.message === 'string') {
            try { value = JSON.parse(value.message); }
            catch {
                if (value.result === 'definitely_not_started') return { key: 'batchNotStarted', count: 0 };
                if (value.result === 'outcome_unknown') return { key: 'batchOutcomeUnknown', count: 0 };
                return null;
            }
        }
        if (value.status === 'completed' && Number.isInteger(value.completed_steps)) return { key: 'batchCompleted', count: value.completed_steps };
        if (value.status === 'stopped_on_error' && Number.isInteger(value.failed_step_number)) {
            const counted = value.failed_step_number > 0 && value.failed_step_number <= 20
                && value.completed_steps === value.failed_step_number - 1;
            const stage = counted && value.effect === 'may_have_effect' ? 'batchOutcomeUnknown'
                : counted && value.effect === 'no_effect' ? (value.completed_steps === 0 ? 'batchNotStarted' : 'batchPartiallyDispatched') : null;
            return { key: 'batchFailed', count: value.failed_step_number, stage, completed: value.completed_steps };
        }
    } catch { /* Unstructured transport errors use the original payload. */ }
    return null;
}

export function AssistantToolCall({ tool, running }: { tool?: AiAssistantToolActivity; running: boolean }) {
    const { t } = useTranslation();
    const [open, setOpen] = useState(false);
    if (!tool) return null;
    const prefix = 'pages.aiAssistant.toolCall.';
    const batch = batchResult(tool);
    const permissionSkip = isHistoricalPermissionSkip(tool.output);
    const permissionSubmission = isPermissionSubmissionReceipt(tool);
    const outcomeUnknown = hasUnknownActionResult(tool.output, tool.callId);
    const status = tool.status === 'running' && !running ? 'missing' : tool.status;
    const StatusIcon = permissionSkip || outcomeUnknown ? CircleHelp : status === 'ok' ? CheckCircle2 : status === 'failed' ? XCircle : status === 'running' ? Loader2 : CircleHelp;
    const statusLabel = t(`${prefix}${permissionSkip ? 'skippedForPermission' : outcomeUnknown ? 'outcomeUnknown' : status === 'ok' ? 'success' : status === 'failed' ? 'failure' : status === 'running' ? 'waiting' : 'missing'}`);
    const statusClass = permissionSkip || outcomeUnknown ? 'text-amber-700 dark:text-amber-300' : status === 'ok' ? 'text-green-600 dark:text-green-400' : status === 'failed' ? 'text-destructive' : 'text-muted-foreground';
    return <Disclosure open={open} onOpenChange={setOpen} className="min-w-0 rounded-md border bg-muted/30 px-3 py-2" title={<>
            <span role="img" aria-label={statusLabel} title={statusLabel} className={`mr-2 inline-flex align-middle ${statusClass}`}>
                <StatusIcon aria-hidden="true" className={`size-4 shrink-0${status === 'running' && !permissionSkip && !outcomeUnknown ? ' animate-spin motion-reduce:animate-none' : ''}`} />
            </span>
            {t(`${prefix}title`)} · {tool.name === 'unknown' ? t(`${prefix}unknown`) : tool.name}
            {permissionSkip && <> · {statusLabel}</>}
            {outcomeUnknown && !batch && <> · {t(`${prefix}inspectBeforeRetry`)}</>}
            {batch && <> · {t(`${prefix}${batch.key}`, { count: batch.count })}</>}
            {batch?.stage && <> · {t(`${prefix}${batch.stage}`, { count: batch.completed })}</>}
        </>} summaryClassName="cursor-pointer select-none break-words text-sm [overflow-wrap:anywhere]">

        {open && <div className="mt-3 min-w-0 space-y-3 text-xs">
            {permissionSkip && <p className="text-amber-700 dark:text-amber-300">{t('pages.aiAssistant.historicalPermissionSkip')}</p>}
            {permissionSubmission && <p className="text-muted-foreground">{t('pages.aiAssistant.permissionSubmissionReceipt')}</p>}
            {tool.permissionReason && <p className="whitespace-pre-wrap break-words">{t('pages.aiAssistant.permissionReasonLabel', { reason: tool.permissionReason })}</p>}
            <AssistantCodeBlock label={t(`${prefix}input`)} text={tool.name === 'unknown' ? t(`${prefix}missingInput`) : tool.argumentsJson} />
            <AssistantCodeBlock label={t(`${prefix}output`)} text={tool.output === null
                ? t(`${prefix}${running && tool.status === 'running' ? 'waiting' : 'missing'}`)
                : tool.output === '' ? t(`${prefix}empty`) : tool.output} />
        </div>}
    </Disclosure>;
}
