import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import type { DeviceAssistantToolActivity } from '@/features/desk/use-device-assistant-chat';

export function ScheduleProposalCards({ tools }: { tools: DeviceAssistantToolActivity[] }) {
    const { t } = useTranslation();
    const ids = new Set<string>();
    for (const tool of tools) {
        if (tool.name !== 'request_scheduled_task' || tool.status !== 'ok' || !tool.output || tool.output.length > 8192) continue;
        try {
            const value = JSON.parse(tool.output);
            if (value.state === 'draft' && ['fresh_task', 'conversation_resume'].includes(value.kind)
                && typeof value.schedule_id === 'string' && /^[a-f0-9-]{36}$/.test(value.schedule_id)) ids.add(value.schedule_id);
        } catch { /* Non-proposal output remains in the ordinary activity view. */ }
    }
    return <>{[...ids].map(id => <article key={id} className="space-y-2 rounded-lg border p-3">
        <p className="font-medium">{t('schedules.proposal.created')}</p>
        <p className="text-sm text-muted-foreground">{t('schedules.proposal.note')}</p>
        <Button variant="outline" asChild><Link to={`/schedules?${new URLSearchParams({ review_task: id })}`}>{t('schedules.proposal.open')}</Link></Button>
    </article>)}</>;
}
