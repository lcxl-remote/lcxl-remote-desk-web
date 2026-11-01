import { Bot } from 'lucide-react';
import { Fragment, useEffect, useRef, type ComponentProps } from 'react';
import { matchesAssistantMessage, type AssistantMessageFocus } from './assistant-message-history';
import { useTranslation } from 'react-i18next';
import { MarkdownContent } from '@/components/markdown-content';
import { assistantCitations } from './assistant-data-api';
import type { ContextNoticeDto } from '@/services/types';
import type { AiAssistantMessage, AiAssistantToolActivity } from './use-ai-assistant-chat';
import { AssistantImages } from './assistant-images';
import { AssistantReasoning } from './assistant-reasoning';
import { AssistantToolGroup } from './assistant-tool-group';
import { AssistantToolCall, isHistoricalPermissionSkip } from './assistant-tool-call';
import { AssistantCommandResult } from './assistant-command-result';
import { AssistantResultAttachments } from './assistant-attachments';
import { AssistantContextNotices, noticeMessageId } from './assistant-context-notices';

/** Main and delegated sessions share presentation, never their input authority. */
export function AssistantTranscript({ sessionId, messages, tools, running, evidence = [], notices = [], displayNameForTool, exportBackup, focus, readOnly = false }: {
    focus?: AssistantMessageFocus; readOnly?: boolean;
    sessionId?: string; messages: AiAssistantMessage[]; tools: AiAssistantToolActivity[]; running: boolean;
    evidence?: ComponentProps<typeof AssistantImages>['evidence']; notices?: ContextNoticeDto[]; displayNameForTool?: (name: string) => string;
    exportBackup?: (id: string) => Promise<void>;
}) {
    const { t } = useTranslation();
    const focusedId = messages.find(message => message.id === focus?.messageId)?.id
        ?? messages.find(message => matchesAssistantMessage(message, focus?.messageId))?.id;
    const target = useRef<HTMLDivElement | null>(null);
    useEffect(() => {
        if (focus?.scroll === false) return;
        target.current?.scrollIntoView?.({ block: 'center' });
        target.current?.focus({ preventScroll: true });
    }, [focus?.messageId, focus?.scroll, focusedId]);
    const callElementId = (id: string) => `assistant-call-${sessionId ?? 'local'}-${id}`;
    const displayNameForCall = (id?: string) => {
        const name = tools.find(tool => tool.callId === id)?.name;
        return name && displayNameForTool ? displayNameForTool(name) : undefined;
    };
    const renderTranscriptMessage = (message: AiAssistantMessage) => (
        <Fragment key={message.id}>
        {(message.role !== 'assistant' || message.text || message.reasoning) && <div
            key={message.id}
            id={message.role === 'tool_call' ? callElementId(message.toolCallId!) : undefined}
            tabIndex={message.id === focusedId || message.role === 'tool_call' ? -1 : undefined}
            ref={message.id === focusedId ? node => { target.current = node; } : undefined}
            data-source-anchor={message.id === focusedId || undefined}
            data-assistant-message-id={message.id}
            className={`rounded-lg px-3 py-2 text-sm ${message.id === focusedId ? 'ring-2 ring-primary ring-offset-2' : ''} ${
                message.role === 'user'
                    ? 'ml-auto max-w-[90%] bg-muted'
                    : message.role === 'tool_result' ? 'w-full max-w-full border bg-muted/30 sm:max-w-[90%]' : 'w-full max-w-full bg-transparent sm:max-w-[90%]'
            }`}
        >
            {message.role === 'goal_decision' && message.goalDecision ? <div role="status" className="space-y-1 border-l-2 border-muted-foreground/40 pl-3">
                <p className="flex items-center gap-2 font-medium"><Bot className="h-4 w-4" aria-hidden="true" />
                    {t(message.goalDecision.reviewer === 'system' ? 'pages.aiAssistant.goalAiReviewFailed'
                        : message.goalDecision.status === 'approved' ? 'pages.aiAssistant.goalAiReviewApproved' : 'pages.aiAssistant.goalAiReviewDenied')}</p>
                <p className="whitespace-pre-wrap text-muted-foreground">{message.text}</p>
            </div> : message.role === 'tool_call' ? <AssistantToolCall tool={tools.find(tool => tool.callId === message.toolCallId)} running={running}
                displayName={displayNameForCall(message.toolCallId)} initialOpen={message.id === focusedId} /> : message.role === 'tool_result' ? <>
                {message.permissionReason && <p className="mb-2 text-sm">{t('pages.aiAssistant.permissionReasonLabel', { reason: message.permissionReason })}</p>}
                {isHistoricalPermissionSkip(message.text) && <p className="mb-2 text-sm text-amber-700 dark:text-amber-300">{t('pages.aiAssistant.historicalPermissionSkip')}</p>}
                <AssistantCommandResult text={message.text} tool={tools.find(tool => tool.callId === message.toolCallId)} onLocateCall={message.toolCallId ? () => { const target = document.getElementById(callElementId(message.toolCallId!)); target?.scrollIntoView({ block: 'center', behavior: 'smooth' }); target?.focus({ preventScroll: true }); } : undefined} onExportBackup={exportBackup} />
                <AssistantResultAttachments sessionId={sessionId} text={message.text} />
            </> : message.role === 'assistant'
                ? <><AssistantReasoning text={message.reasoning} />{message.text && <MarkdownContent disableLinks assistantSources={assistantCitations(message)}>{message.text}</MarkdownContent>}
                    {message.citationsIncomplete && <p className="text-xs text-muted-foreground">{t('pages.aiAssistant.memory.citationsIncomplete')}</p>}</>
                : <p className="whitespace-pre-wrap">{message.id === focusedId && focus?.parts
                    ? <>{focus.parts.before}<mark className="bg-primary/20 text-foreground">{focus.parts.match}</mark>{focus.parts.after}</> : message.text}</p>}
        </div>}
        <AssistantContextNotices notices={notices.filter(notice => noticeMessageId(notice, messages) === message.id)} />
        </Fragment>
                        );

    return <div data-testid="assistant-shared-transcript" className="min-h-48 space-y-5 py-4">
        <AssistantImages key={sessionId} sessionId={sessionId} evidence={evidence} readOnly={readOnly} focusedMessageId={focusedId}
            messages={messages} renderMessage={renderTranscriptMessage}
            renderReasoning={message => <div className="w-full max-w-full text-sm sm:max-w-[90%]"><AssistantReasoning text={message.reasoning} /></div>}
            renderToolGroup={group => <AssistantToolGroup messages={group} tools={tools}
                renderMessage={renderTranscriptMessage} displayNameForTool={displayNameForTool}
                focused={group.some(message => message.id === focusedId)} />} />
        <AssistantContextNotices historical notices={notices.filter(notice => !noticeMessageId(notice, messages))} />
    </div>;
}
