import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import type { AiAssistantGoal, AiAssistantGoalOwnerAction } from './use-ai-assistant-chat';

export function goalPlanningAction(goal: Pick<AiAssistantGoal, 'state' | 'pauseReason'>): AiAssistantGoalOwnerAction | null {
    if (['completed', 'failed', 'cancelled', 'waiting_user'].includes(goal.state)) return null;
    if (goal.state === 'paused') return goal.pauseReason === 'stalled' ? 'retry_stalled' : 'resume';
    return ['queued', 'running', 'waiting_approval', 'waiting_work', 'waiting_device', 'waiting_model', 'blocked'].includes(goal.state) ? 'pause' : null;
}

export function goalProgressMessage(goal: Pick<AiAssistantGoal, 'state' | 'statusReason' | 'checkpointSummary'>): string | null {
    if (goal.state === 'waiting_user') return goal.statusReason?.trim() || null;
    if (goal.state === 'blocked') return goal.statusReason?.trim() || null;
    const summary = goal.checkpointSummary?.trim();
    return summary && !summary.startsWith('Waiting for ') ? summary : null;
}

export function AssistantGoalPanel({ goal, connected, main, enabled, busy, onDetails, onAction, onReply, onContinue }: {
    goal: AiAssistantGoal;
    connected: boolean;
    main: boolean;
    enabled: boolean;
    busy: boolean;
    onDetails: () => void;
    onAction: (action: AiAssistantGoalOwnerAction) => void;
    onReply: () => void;
    onContinue: () => void;
}) {
    const { t } = useTranslation();
    const action = goalPlanningAction(goal);
    const message = goalProgressMessage(goal);
    const terminal = ['completed', 'failed', 'cancelled'].includes(goal.state);
    return <section aria-label={t('pages.aiAssistant.goalPanelTitle')} data-testid="assistant-goal-panel"
        className="mx-auto w-full max-w-[840px] shrink-0 rounded-lg border bg-muted/30 p-3 text-sm">
        <div className="flex flex-wrap items-center gap-2">
            <span className="font-medium">{t(main ? 'pages.aiAssistant.goalPanelTitle' : 'pages.aiAssistant.goalMainTitle')}</span>
            <Badge variant="outline" role="status" aria-live="polite">{t(`pages.aiAssistant.goalStates.${goal.state}`)}</Badge>
            {!connected && <span className="text-xs text-muted-foreground">{t('pages.aiAssistant.goalLastKnown')}</span>}
            <Button type="button" size="sm" variant="ghost" className="ml-auto h-7" onClick={onDetails}>
                {t('pages.aiAssistant.goalDetails')}
            </Button>
        </div>
        <p className="mt-1 max-h-24 overflow-y-auto whitespace-pre-wrap break-words [overflow-wrap:anywhere]">{goal.goalText}</p>
        {message && <p role="status" aria-live="polite" className="mt-2 max-h-24 overflow-y-auto whitespace-pre-wrap break-words text-xs text-muted-foreground">
            {/^[a-z][a-z0-9_]*$/.test(message)
                ? t(`pages.aiAssistant.goalReasons.${message}`, { defaultValue: message }) : message}
        </p>}
        {main && <div className="mt-2 flex flex-wrap gap-2">
            {goal.state === 'waiting_user' && <Button type="button" size="sm" variant="outline" disabled={!enabled || busy} onClick={onReply}>
                {t('pages.aiAssistant.goalReply')}
            </Button>}
            {action && <Button type="button" size="sm" variant="outline" disabled={!enabled || busy} onClick={() => onAction(action)}>
                {t(action === 'retry_stalled' ? 'pages.aiAssistant.goalRetry' : action === 'resume' ? 'pages.aiAssistant.goalResume' : 'pages.aiAssistant.goalPause')}
            </Button>}
            {!terminal && <Button type="button" size="sm" variant="ghost" disabled={!enabled || busy} onClick={() => onAction('cancel')}>
                {t('pages.aiAssistant.goalCancel')}
            </Button>}
            {goal.state === 'completed' && <Button type="button" size="sm" variant="outline" disabled={!enabled || busy} onClick={onContinue}>
                {t('pages.aiAssistant.goalStillIncomplete')}
            </Button>}
        </div>}
    </section>;
}
