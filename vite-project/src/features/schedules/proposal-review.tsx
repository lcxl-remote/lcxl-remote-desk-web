import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Check, X } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import type { ScheduleView } from '@/services/types';
import { ScheduleClient, ScheduleRequestError, scheduleErrorMessage } from './client';
import { formatTime, ruleTimes, validTimezone } from './time';

/** One pending AI fresh-task proposal; the owner approves or rejects it as proposed. */
export function ProposalReview({ client, scheduleId, connected, zone, onChanged, onLoaded, activationDisabled = false, approvalCard = false }: {
    client: ScheduleClient; scheduleId: string; connected: boolean; zone: string;
    approvalCard?: boolean; activationDisabled?: boolean; onChanged: (task: ScheduleView) => void;
    onLoaded?: (task: ScheduleView) => void;
}) {
    const { t, i18n } = useTranslation();
    const [task, setTask] = useState<ScheduleView | null>(null);
    const [error, setError] = useState('');
    const [busy, setBusy] = useState(false);
    const [refresh, setRefresh] = useState(0);
    const epoch = useRef(0);
    const pending = useRef(false);
    useEffect(() => {
        const current = ++epoch.current;
        setTask(null); setError(''); setBusy(connected); pending.current = false;
        if (connected) void client.request({ operation: 'get', schedule_id: scheduleId }).then(response => {
            if (current !== epoch.current) return;
            if (response.result !== 'task' || response.task.schedule_id !== scheduleId) throw new ScheduleRequestError('invalid');
            setTask(response.task); onLoaded?.(response.task);
        }).catch(() => { if (current === epoch.current) setError(t('schedules.requestFailed')); })
            .finally(() => { if (current === epoch.current) setBusy(false); });
        return () => { ++epoch.current; };
    }, [client, scheduleId, connected, refresh, t]);
    const decide = async (approve: boolean) => {
        if (activationDisabled || !task || !connected || busy || pending.current || task.status !== 'pending_review') return;
        const current = epoch.current;
        pending.current = true; setBusy(true); setError('');
        try {
            const response = await client.request({ operation: 'decide_schedule_proposal', schedule_id: task.schedule_id, expected_revision: task.revision, approve });
            if (current !== epoch.current) return;
            if (response.result !== 'task' || response.task.schedule_id !== scheduleId) throw new ScheduleRequestError('invalid');
            setTask(response.task); onChanged(response.task);
        } catch (reason) {
            if (current === epoch.current) setError(scheduleErrorMessage(reason, t));
        } finally { if (current === epoch.current) { pending.current = false; setBusy(false); } }
    };
    if (approvalCard && task && task.status !== 'pending_review') return null;
    const timeZone = validTimezone(zone) ? zone : 'UTC';
    return <div className="space-y-3">
        {(!approvalCard || !!error) && <Button variant="outline" disabled={!connected || busy} onClick={() => setRefresh(value => value + 1)}>{t('schedules.refresh')}</Button>}
        {!connected && <p role="status" className="text-xs text-muted-foreground">{t('schedules.connecting')}</p>}
        {busy && <p role="status" className="text-xs text-muted-foreground">{t('schedules.loading')}</p>}
        {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
        {task && <div className={approvalCard ? 'min-w-0 space-y-3 rounded-md bg-muted/50 p-3' : 'space-y-3'}>
            <div className="flex flex-wrap items-center justify-between gap-2">
                <h3 className="min-w-0 break-words text-sm font-medium">{task.title}</h3>
                <Badge variant={task.status === 'pending_review' ? 'default' : 'outline'}>{t(`schedules.status.${task.status}`)}</Badge>
            </div>
            <div className="space-y-3 rounded border bg-background px-3 py-2">
                <div className="space-y-1">
                    <p className="text-xs text-muted-foreground">{t('schedules.prompt')}</p>
                    <p className="max-h-48 overflow-y-auto whitespace-pre-wrap break-words text-sm">{task.prompt}</p>
                </div>
                <div className="space-y-1 border-t pt-2">
                    <p className="text-xs text-muted-foreground">{t('schedules.proposal.executionTime')}</p>
                    <p className="text-sm">{task.spec.rule.kind === 'interval'
                        ? t('schedules.everySeconds', { count: task.spec.rule.every_seconds })
                        : `${t(`schedules.rule.${task.spec.rule.kind}`)} · ${ruleTimes(task.spec, timeZone, i18n.language).join(' / ')}`}</p>
                    {task.upcoming_runs.slice(0, 3).map(at => <p key={at} className="text-xs text-muted-foreground">{formatTime(at, timeZone, i18n.language)}</p>)}
                </div>
            </div>
            <p className="text-xs text-muted-foreground">{t('schedules.proposal.freshNote')}</p>
            {task.status === 'pending_review' && <div className="flex flex-wrap gap-2">
                <Button type="button" size="sm" disabled={!connected || busy || activationDisabled} onClick={() => void decide(true)}><Check className="mr-2 h-4 w-4" />{t('schedules.proposal.approve')}</Button>
                <Button type="button" size="sm" variant="outline" disabled={!connected || busy || activationDisabled} onClick={() => void decide(false)}><X className="mr-2 h-4 w-4" />{t('schedules.proposal.reject')}</Button>
            </div>}
        </div>}
    </div>;
}
