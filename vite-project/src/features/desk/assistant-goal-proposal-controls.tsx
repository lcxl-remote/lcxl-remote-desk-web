import { useTranslation } from 'react-i18next';
import { Bot } from 'lucide-react';
import { Button } from '@/components/ui/button';

export function AssistantGoalProposalControls({ automaticApproval, revision, busy, canApprove, onDecide }: {
    automaticApproval: boolean;
    revision: boolean;
    busy: boolean;
    canApprove: boolean;
    onDecide: (approve: boolean) => unknown;
}) {
    const { t } = useTranslation();
    if (automaticApproval) return <p role="status" className="mt-2 flex items-center gap-2 text-muted-foreground">
        <Bot className="h-4 w-4 shrink-0" aria-hidden="true" />{t('pages.aiAssistant.goalAiReviewPending')}
    </p>;
    return <div className="mt-2 flex gap-2">
        <Button size="sm" disabled={busy || !canApprove} onClick={() => void onDecide(true)}>
            {t(revision ? 'pages.aiAssistant.goalRevisionApprove' : 'pages.aiAssistant.goalProposalApprove')}
        </Button>
        <Button size="sm" variant="outline" disabled={busy} onClick={() => void onDecide(false)}>
            {t('pages.aiAssistant.goalProposalDeny')}
        </Button>
    </div>;
}
