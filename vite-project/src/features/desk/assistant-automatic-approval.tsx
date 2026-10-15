import { useEffect, useId } from 'react';
import { useTranslation } from 'react-i18next';
import { Bot } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Switch } from '@/components/ui/switch';
import type { useAiAssistantChat } from './use-ai-assistant-chat';

type ApprovalChat = Pick<ReturnType<typeof useAiAssistantChat>,
    'conversationId' | 'sessionId' | 'hydrating' | 'contextUpdating' | 'approvalUpdating' | 'approvalDelegation'
    | 'approvalModelReadiness' | 'turnRunning' | 'error' | 'approvalInitializing' | 'approvalInitializationFailed'
    | 'initializeApprovalConversation' | 'setAutomaticApproval'>;

export function AssistantAutomaticApproval({ chat, enabled, connected }: {
    chat: ApprovalChat;
    enabled: boolean;
    connected: boolean;
}) {
    const { t } = useTranslation();
    const switchId = useId();
    useEffect(() => {
        if (enabled && connected && (!chat.sessionId || !chat.approvalModelReadiness) && !chat.hydrating && !chat.contextUpdating
            && !chat.approvalInitializing && !chat.approvalInitializationFailed && !chat.error) {
            void chat.initializeApprovalConversation();
        }
    }, [enabled, connected, chat.sessionId, chat.approvalModelReadiness, chat.hydrating, chat.contextUpdating,
        chat.approvalInitializing, chat.approvalInitializationFailed, chat.error, chat.initializeApprovalConversation]);
    const active = chat.approvalDelegation?.status === 'active';
    return <div data-testid="ai-assistant-automatic-approval" className="rounded-md border px-3 py-2 text-xs">
        <div className="flex items-center justify-between gap-3">
            <div className="min-w-0">
                <label htmlFor={switchId} className="flex items-center gap-2 font-medium"><Bot className="h-4 w-4" aria-hidden="true" />{t('pages.aiAssistant.autoApprovalTitle')}</label>
                <p id={`${switchId}-description`} className="mt-1 text-muted-foreground">{t('pages.aiAssistant.autoApprovalDescription')}</p>
            </div>
            <Switch id={switchId} checked={active} aria-label={t('pages.aiAssistant.autoApprovalTitle')}
                aria-describedby={`${switchId}-description`} aria-busy={chat.approvalUpdating}
                disabled={!enabled || !chat.sessionId || chat.hydrating || chat.contextUpdating || chat.approvalInitializing
                    || chat.approvalUpdating || (!active && (!chat.approvalModelReadiness?.available || chat.turnRunning))}
                onCheckedChange={checked => void chat.setAutomaticApproval(checked)} />
        </div>
        {chat.approvalUpdating && <p role="status" className="mt-2 text-muted-foreground">{t('common.loading')}</p>}
        {(chat.approvalInitializationFailed || chat.error) && <div role="alert" className="mt-2 text-destructive">
            <p>{chat.approvalInitializationFailed ? t('pages.aiAssistant.approvalInitializationFailed')
                : chat.error === 'history_restore_failed' ? t('pages.aiAssistant.history.restoreError') : chat.error}</p>
            {(!chat.sessionId || !chat.approvalModelReadiness) && <Button size="sm" variant="outline" className="mt-2"
                disabled={!enabled || !connected || chat.hydrating || chat.contextUpdating || chat.approvalInitializing}
                onClick={() => void chat.initializeApprovalConversation(true)}>{t('pages.aiAssistant.history.retry')}</Button>}
        </div>}
        {active && chat.approvalDelegation
            ? <p className="mt-1 text-muted-foreground">{t('pages.aiAssistant.autoApprovalUsage', {
                reviews: chat.approvalDelegation.reviewsUsed, tokens: chat.approvalDelegation.tokensUsed,
            })}</p>
            : chat.approvalModelReadiness && !chat.approvalModelReadiness.available
                ? <p className="mt-1 text-amber-700 dark:text-amber-300">{t('pages.aiAssistant.autoApprovalUnavailable')}: {t(`pages.aiAssistant.approvalModelReason.${chat.approvalModelReadiness.reason ?? 'unknown'}`)}</p>
                : !chat.approvalUpdating && !chat.error && !chat.approvalInitializationFailed && enabled && connected
                    && (chat.contextUpdating || chat.hydrating || chat.approvalInitializing || !chat.approvalModelReadiness)
                    ? <p role="status" className="mt-2 text-muted-foreground">{t('common.loading')}</p> : null}
    </div>;
}
