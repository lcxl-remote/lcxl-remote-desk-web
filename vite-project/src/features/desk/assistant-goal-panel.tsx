import { useId, useState } from 'react';
import { ChevronDown, ChevronUp, Target, WifiOff } from 'lucide-react';
import { useTranslation } from 'react-i18next';
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
    const [expanded, setExpanded] = useState(false);
    const contentId = useId();
    const title = t(main ? 'pages.aiAssistant.goalPanelTitle' : 'pages.aiAssistant.goalMainTitle');
    const action = goalPlanningAction(goal);
    const message = goalProgressMessage(goal);
    const terminal = ['completed', 'failed', 'cancelled'].includes(goal.state);
    return <section aria-label={title} data-testid="assistant-goal-panel"
        className="mx-auto w-full min-w-0 max-w-[840px] shrink-0 rounded-lg border bg-muted/30 text-sm">
        <button type="button" aria-expanded={expanded} aria-controls={contentId}
            aria-label={t(expanded ? 'pages.aiAssistant.goalCollapse' : 'pages.aiAssistant.goalExpand')}
            aria-describedby={`${contentId}-summary ${contentId}-state${connected ? '' : ` ${contentId}-offline`}`}
            className="flex min-h-11 w-full min-w-0 items-center gap-2 rounded-lg px-3 py-2 text-left hover:bg-muted/50 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
            onClick={() => setExpanded(value => !value)}>
            <Target aria-hidden="true" className="h-4 w-4 shrink-0" />
            <span className="sr-only sm:not-sr-only sm:shrink-0 sm:font-medium">{title}</span>
            <span id={`${contentId}-summary`} className="min-w-0 flex-1 truncate" title={goal.goalText}>{goal.goalText.replace(/\s+/g, ' ').trim()}</span>
            <span id={`${contentId}-state`} role="status" aria-live="polite" className="max-w-[45%] shrink-0 truncate rounded-md border px-2 py-0.5 text-xs font-medium">{t(`pages.aiAssistant.goalStates.${goal.state}`)}</span>
            {!connected && <span title={t('pages.aiAssistant.goalLastKnown')} className="shrink-0">
                <WifiOff aria-hidden="true" className="h-4 w-4 text-muted-foreground" />
                <span id={`${contentId}-offline`} className="sr-only">{t('pages.aiAssistant.goalLastKnown')}</span>
            </span>}
            {expanded ? <ChevronUp aria-hidden="true" className="h-4 w-4 shrink-0" /> : <ChevronDown aria-hidden="true" className="h-4 w-4 shrink-0" />}
        </button>
        <div id={contentId} hidden={!expanded} className="max-h-[min(16rem,30dvh)] overflow-y-auto overscroll-contain px-3 pb-3 [overflow-wrap:anywhere]">
            {expanded && <>
                <p className="whitespace-pre-wrap break-words">{goal.goalText}</p>
                {message && <p role="status" aria-live="polite" className="mt-2 whitespace-pre-wrap break-words text-xs text-muted-foreground">
                    {/^[a-z][a-z0-9_]*$/.test(message)
                        ? t(`pages.aiAssistant.goalReasons.${message}`, { defaultValue: message }) : message}
                </p>}
                <div className="mt-2 flex flex-wrap gap-2">
                    <Button type="button" size="sm" variant="ghost" onClick={onDetails}>{t('pages.aiAssistant.goalDetails')}</Button>
                    {main && <>
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
                    </>}
                </div>
            </>}
        </div>
    </section>;
}
