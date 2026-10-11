import { useTranslation } from 'react-i18next';
import { Bot, Layers, ToggleRight, X } from 'lucide-react';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';

export function AssistantComposerStatus({ automaticApproval, onApprovalSettings, contextCount = 0, onContext, startGoal = false, onClearGoal }: {
    automaticApproval: boolean;
    onApprovalSettings?: () => void;
    contextCount?: number;
    onContext?: () => void;
    startGoal?: boolean;
    onClearGoal?: () => void;
}) {
    const { t } = useTranslation();
    const label = t('pages.aiAssistant.workspace.autoApprovalActive');
    const contextLabel = t('pages.aiAssistant.workspace.contextCount', { count: contextCount });
    const goalLabel = t('pages.aiAssistant.goalStart');
    const approval = <><Bot className="h-4 w-4 shrink-0" aria-hidden="true" /><span className="assistant-action-label truncate">{label}</span></>;
    return <div data-testid="assistant-composer-status" className="flex min-w-0 flex-1 flex-wrap items-center gap-1">
        {contextCount > 0 && onContext && <Button type="button" size="sm" variant="secondary" className="assistant-status-action min-w-0 max-w-full gap-1"
            title={contextLabel} aria-label={contextLabel} aria-haspopup="dialog" onClick={onContext}>
            <Layers className="h-4 w-4 shrink-0" aria-hidden="true" /><span className="assistant-action-label truncate">{contextLabel}</span>
        </Button>}
        {startGoal && onClearGoal && <Button type="button" size="sm" variant="secondary" className="assistant-status-action min-w-0 max-w-full gap-1"
            title={goalLabel} aria-label={goalLabel} aria-pressed onClick={onClearGoal}>
            <ToggleRight className="h-4 w-4 shrink-0" aria-hidden="true" /><span className="assistant-action-label truncate">{goalLabel}</span>
            <X className="assistant-action-label h-3.5 w-3.5 shrink-0" aria-hidden="true" />
        </Button>}
        {automaticApproval && (onApprovalSettings
            ? <Button type="button" size="sm" variant="secondary" className="assistant-status-action min-w-0 max-w-full gap-1"
                title={label} aria-label={label} aria-haspopup="dialog" onClick={onApprovalSettings}>{approval}</Button>
            : <Badge variant="secondary" className="assistant-status-action min-w-0 max-w-full gap-1" role="status" title={label} aria-label={label}>{approval}</Badge>)}
    </div>;
}
