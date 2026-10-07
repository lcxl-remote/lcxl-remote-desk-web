import { Fragment, type ComponentProps } from 'react';
import { useTranslation } from 'react-i18next';
import { MarkdownContent } from '@/components/markdown-content';
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
export function AssistantTranscript({ sessionId, messages, tools, running, evidence = [], notices = [], displayNameForTool, exportBackup }: {
    sessionId?: string; messages: AiAssistantMessage[]; tools: AiAssistantToolActivity[]; running: boolean;
    evidence?: ComponentProps<typeof AssistantImages>['evidence']; notices?: ContextNoticeDto[]; displayNameForTool?: (name: string) => string;
    exportBackup?: (id: string) => Promise<void>;
}) {
    const { t } = useTranslation();
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
            tabIndex={message.role === 'tool_call' ? -1 : undefined}
            className={`rounded-lg px-3 py-2 text-sm ${
                message.role === 'user'
                    ? 'ml-auto max-w-[90%] bg-muted'
                    : message.role === 'tool_result' ? 'w-full max-w-full border bg-muted/30 sm:max-w-[90%]' : 'w-full max-w-full bg-transparent sm:max-w-[90%]'
            }`}
        >
            {message.role === 'tool_call' ? <AssistantToolCall tool={tools.find(tool => tool.callId === message.toolCallId)} running={running}
                displayName={displayNameForCall(message.toolCallId)} /> : message.role === 'tool_result' ? <>
                {message.permissionReason && <p className="mb-2 text-sm">{t('pages.aiAssistant.permissionReasonLabel', { reason: message.permissionReason })}</p>}
                {isHistoricalPermissionSkip(message.text) && <p className="mb-2 text-sm text-amber-700 dark:text-amber-300">{t('pages.aiAssistant.historicalPermissionSkip')}</p>}
                <AssistantCommandResult text={message.text} tool={tools.find(tool => tool.callId === message.toolCallId)} onLocateCall={message.toolCallId ? () => { const target = document.getElementById(callElementId(message.toolCallId!)); target?.scrollIntoView({ block: 'center', behavior: 'smooth' }); target?.focus({ preventScroll: true }); } : undefined} onExportBackup={exportBackup} />
                <AssistantResultAttachments sessionId={sessionId} text={message.text} />
            </> : message.role === 'assistant'
                ? <><AssistantReasoning text={message.reasoning} />{message.text && <MarkdownContent disableLinks>{message.text}</MarkdownContent>}</>
                : <p className="whitespace-pre-wrap">{message.text}</p>}
        </div>}
        <AssistantContextNotices notices={notices.filter(notice => noticeMessageId(notice, messages) === message.id)} />
        </Fragment>
                        );

    return <div data-testid="assistant-shared-transcript" className="min-h-48 space-y-5 py-4">
        <AssistantImages key={sessionId} sessionId={sessionId} evidence={evidence}
            messages={messages} renderMessage={renderTranscriptMessage}
            renderReasoning={message => <div className="w-full max-w-full text-sm sm:max-w-[90%]"><AssistantReasoning text={message.reasoning} /></div>}
            renderToolGroup={group => <AssistantToolGroup messages={group} tools={tools}
                renderMessage={renderTranscriptMessage} displayNameForTool={displayNameForTool} />} />
        <AssistantContextNotices historical notices={notices.filter(notice => !noticeMessageId(notice, messages))} />
    </div>;
}
