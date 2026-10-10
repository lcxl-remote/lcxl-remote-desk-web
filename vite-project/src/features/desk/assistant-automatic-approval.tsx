import { useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { ShieldCheck } from 'lucide-react';
import { Button } from '@/components/ui/button';
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
                <p className="flex items-center gap-2 font-medium"><ShieldCheck className="h-4 w-4" />{t('pages.aiAssistant.autoApprovalTitle')}</p>
                <p className="mt-1 text-muted-foreground">{t('pages.aiAssistant.autoApprovalDescription')}</p>
            </div>
            <Button size="sm" variant="outline"
                disabled={!enabled || !chat.sessionId || chat.hydrating || chat.contextUpdating || chat.approvalInitializing
                    || chat.approvalUpdating || (!active && (!chat.approvalModelReadiness?.available || chat.turnRunning))}
                onClick={() => void chat.setAutomaticApproval(!active)}>
                {t(active ? 'pages.aiAssistant.autoApprovalDisable' : 'pages.aiAssistant.autoApprovalEnable')}
            </Button>
        </div>
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
                : !chat.error && !chat.approvalInitializationFailed && enabled && connected
                    && (chat.contextUpdating || chat.hydrating || chat.approvalInitializing || !chat.approvalModelReadiness)
                    ? <p role="status" className="mt-2 text-muted-foreground">{t('common.loading')}</p> : null}
    </div>;
}
