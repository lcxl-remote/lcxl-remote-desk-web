import type { ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { AlertTriangle } from 'lucide-react';
import { Disclosure } from '@/components/ui/disclosure';
import type { AiAssistantMessage, AiAssistantToolActivity } from './use-ai-assistant-chat';
import { hasUnknownActionResult } from './action-result-status';

function failureMessage(output: string | null): string | null {
    if (!output) return null;
    try {
        const value = JSON.parse(output);
        if (!value || typeof value !== 'object') return null;
        const record = value as Record<string, unknown>;
        const nested = [record.error, record.failure, record.output].filter(
            (item): item is Record<string, unknown> => !!item && typeof item === 'object' && !Array.isArray(item));
        const message = [record.message, ...nested.map(item => item.message)].find(
            item => typeof item === 'string' && item.trim() && !item.trim().startsWith('{'));
        return typeof message === 'string' ? message.slice(0, 180) : null;
    } catch {
        const trimmed = output.trimStart();
        return trimmed.startsWith('{') || trimmed.startsWith('[') ? null : output.slice(0, 180);
    }
}

export function AssistantToolGroup({ messages, tools, renderMessage, displayNameForTool }: {
    messages: AiAssistantMessage[];
    tools: AiAssistantToolActivity[];
    renderMessage: (message: AiAssistantMessage) => ReactNode;
    displayNameForTool?: (name: string) => string;
}) {
    const { t } = useTranslation();
    const callIds = [...new Set(messages.map(message => message.toolCallId).filter((id): id is string => Boolean(id)))];
    const entries = callIds.map(id => tools.find(tool => tool.callId === id)).filter((tool): tool is AiAssistantToolActivity => Boolean(tool));
    const uncertain = entries.filter(tool => hasUnknownActionResult(tool.output, tool.callId));
    const known = entries.filter(tool => !uncertain.includes(tool));
    const failed = known.filter(tool => tool.status === 'failed');
    const firstFailureMessage = failureMessage(failed[0]?.output ?? null);
    const succeeded = known.filter(tool => tool.status === 'ok').length;
    const returned = known.filter(tool => tool.status === 'returned').length;
    return <Disclosure className="w-full max-w-full rounded-md border bg-muted/30 px-3 py-2 sm:max-w-[90%]"
        summaryClassName="cursor-pointer text-sm" title={<span className="space-y-1">
            <span className="block font-medium">{t('pages.aiAssistant.workspace.toolGroup', { count: callIds.length })}</span>
            {(succeeded > 0 || returned > 0) && <span className="block text-xs text-muted-foreground">
                {[
                    succeeded > 0 && t('pages.aiAssistant.workspace.toolGroupSucceeded', { count: succeeded }),
                    returned > 0 && t('pages.aiAssistant.workspace.toolGroupReturned', { count: returned }),
                ].filter(Boolean).join(' · ')}
            </span>}
            {failed.length > 0 && <span className="flex items-start gap-1 break-words text-xs text-destructive">
                <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" aria-hidden="true" />
                {t('pages.aiAssistant.workspace.toolGroupFailed', { count: failed.length, name: displayNameForTool?.(failed[0].name) ?? failed[0].name })}
                {firstFailureMessage && <span>{firstFailureMessage}</span>}
            </span>}
            {uncertain.length > 0 && <span className="flex items-start gap-1 text-xs text-amber-700 dark:text-amber-300">
                <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" aria-hidden="true" />
                {t('pages.aiAssistant.workspace.toolGroupOutcomeUnknown', { count: uncertain.length })}
            </span>}
        </span>}>
        <div className="mt-2 space-y-2 border-t pt-2">
            {messages.map(message => <div key={message.id}>{renderMessage(message)}</div>)}
        </div>
    </Disclosure>;
}
