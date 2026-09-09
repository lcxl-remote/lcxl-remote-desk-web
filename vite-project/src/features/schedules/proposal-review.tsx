import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Check, X } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import type { ScheduleView } from '@/services/types';
import { ScheduleClient, ScheduleRequestError } from './client';
import { ContractReview } from './contract-review';
import { RehearsalDetails } from './rehearsal-details';
import { formatTime, validTimezone } from './time';

export function ProposalReview({ client, scheduleId, connected, zone, assistantPaths, onChanged, activationDisabled = false, approvalCard = false }: {
    client: ScheduleClient; scheduleId: string; connected: boolean; zone: string;
    approvalCard?: boolean; activationDisabled?: boolean; assistantPaths: Record<string, string>; onChanged: (task: ScheduleView) => void;
}) {
    const { t, i18n } = useTranslation();
    const [task, setTask] = useState<ScheduleView | null>(null);
    const [view, setView] = useState<'rehearsal' | 'contract'>('rehearsal');
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
            setTask(response.task);
        }).catch(() => { if (current === epoch.current) setError(t('schedules.requestFailed')); })
            .finally(() => { if (current === epoch.current) setBusy(false); });
        return () => { ++epoch.current; };
    }, [client, scheduleId, connected, refresh, t]);
    const decide = async (approve: boolean) => {
        if (activationDisabled || !task || !connected || busy || pending.current || (approve && task.kind !== 'conversation_resume') || task.status !== (task.kind === 'conversation_resume' ? 'pending_review' : 'draft')) return;
        const current = epoch.current;
        pending.current = true; setBusy(true); setError('');
        try {
            const response = await client.request({ operation: approve ? 'activate_conversation_resume' : 'delete', schedule_id: task.schedule_id, expected_revision: task.revision });
            if (current !== epoch.current) return;
            if (response.result !== 'task' || response.task.schedule_id !== scheduleId) throw new ScheduleRequestError('invalid');
            setTask(response.task); onChanged(response.task);
        } catch (reason) {
            if (current === epoch.current) setError(reason instanceof ScheduleRequestError && reason.reason === 'server' ? reason.message : t('schedules.requestFailed'));
        } finally { if (current === epoch.current) { pending.current = false; setBusy(false); } }
    };
    if (approvalCard && task && !['draft', 'pending_review'].includes(task.status)) return null;
    return <div className="space-y-3">
        {(!approvalCard || !!error) && <Button variant="outline" disabled={!connected || busy} onClick={() => setRefresh(value => value + 1)}>{t('schedules.refresh')}</Button>}
        {!connected && <p role="status" className="text-xs text-muted-foreground">{t('schedules.connecting')}</p>}
        {busy && <p role="status" className="text-xs text-muted-foreground">{t('schedules.loading')}</p>}
        {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
        {task && <div className={approvalCard ? 'min-w-0 space-y-3 rounded-md bg-muted/50 p-3' : 'space-y-3'}>
            <div className="flex flex-wrap items-center justify-between gap-2">
                <h3 className="min-w-0 break-words text-sm font-medium">{task.title}</h3>
                <Badge variant={task.status === 'draft' ? 'default' : 'outline'}>{t(`schedules.status.${task.status}`)}</Badge>
            </div>
            <div className="space-y-3 rounded border bg-background px-3 py-2">
                <div className="space-y-1">
                    <p className="text-xs text-muted-foreground">{t('schedules.prompt')}</p>
                    <p className="max-h-48 overflow-y-auto whitespace-pre-wrap break-words text-sm">{task.prompt}</p>
                </div>
                {task.kind === 'conversation_resume' && <div className="space-y-1 border-t pt-2">
                    <p className="text-xs text-muted-foreground">{t('schedules.proposal.executionTime')}</p>
                    {task.spec.rule.kind === 'after_confirmation' && <p className="text-sm">{t('schedules.proposal.afterConfirmation', { seconds: task.spec.rule.delay_seconds })}</p>}
                    {task.spec.rule.kind === 'once' && <p className="text-sm">{formatTime(task.spec.rule.at, validTimezone(zone) ? zone : 'UTC', i18n.language)}</p>}
                </div>}
            </div>
            {task.kind === 'conversation_resume' && <div className="space-y-2">
                <p className="text-xs text-muted-foreground">{t('schedules.proposal.resumeNote')}</p>
                {task.status === 'pending_review' && <div className="flex flex-wrap gap-2">
                    <Button type="button" size="sm" disabled={!connected || busy || activationDisabled} onClick={() => void decide(true)}><Check className="mr-2 h-4 w-4" />{t(approvalCard ? 'schedules.proposal.approve' : 'schedules.activateResume')}</Button>
                    <Button type="button" size="sm" variant="outline" disabled={!connected || busy || activationDisabled} onClick={() => void decide(false)}><X className="mr-2 h-4 w-4" />{t('schedules.proposal.reject')}</Button>
                </div>}
            </div>}
            {task.kind === 'fresh_task' && <>
                {task.status === 'draft' && <Button type="button" size="sm" variant="outline" disabled={!connected || busy || activationDisabled} onClick={() => void decide(false)}><X className="mr-2 size-4" />{t('schedules.proposal.reject')}</Button>}
                <div className="flex gap-2">
                    <Button variant={view === 'rehearsal' ? 'default' : 'outline'} onClick={() => setView('rehearsal')}>{t('schedules.rehearsal.view')}</Button>
                    <Button variant={view === 'contract' ? 'default' : 'outline'} onClick={() => setView('contract')}>{t('schedules.contract.title')}</Button>
                </div>
                {view === 'rehearsal' ? <RehearsalDetails client={client} scheduleId={scheduleId} connected={connected} zone={zone} assistantPaths={assistantPaths} />
                    : <ContractReview client={client} scheduleId={scheduleId} connected={connected} onPublished={value => { setTask(value); onChanged(value); }} />}
            </>}
        </div>}
    </div>;
}
