import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import type { ScheduleView } from '@/services/types';
import { ScheduleClient, ScheduleRequestError } from './client';
import { ContractReview } from './contract-review';
import { RehearsalDetails } from './rehearsal-details';
import { formatTime, validTimezone } from './time';

export function ProposalReview({ client, scheduleId, connected, zone, assistantPaths, onChanged }: {
    client: ScheduleClient; scheduleId: string; connected: boolean; zone: string;
    assistantPaths: Record<string, string>; onChanged: () => void;
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
    const activate = async () => {
        if (!task || !connected || busy || pending.current || task.kind !== 'conversation_resume' || task.status !== 'draft') return;
        const current = epoch.current;
        pending.current = true; setBusy(true); setError('');
        try {
            const response = await client.request({ operation: 'activate_conversation_resume', schedule_id: task.schedule_id, expected_revision: task.revision });
            if (current !== epoch.current) return;
            if (response.result !== 'task' || response.task.schedule_id !== scheduleId) throw new ScheduleRequestError('invalid');
            setTask(response.task); onChanged();
        } catch (reason) {
            if (current === epoch.current) setError(reason instanceof ScheduleRequestError && reason.reason === 'server' ? reason.message : t('schedules.requestFailed'));
        } finally { if (current === epoch.current) { pending.current = false; setBusy(false); } }
    };
    return <div className="space-y-3">
        <Button variant="outline" disabled={!connected || busy} onClick={() => setRefresh(value => value + 1)}>{t('schedules.refresh')}</Button>
        {!connected && <p role="status">{t('schedules.connecting')}</p>}
        {busy && <p role="status">{t('schedules.loading')}</p>}
        {error && <p role="alert">{error}</p>}
        {task && <>
            <h3 className="font-semibold">{task.title}</h3>
            <p className="whitespace-pre-wrap">{task.prompt}</p>
            <p>{t(`schedules.status.${task.status}`)}</p>
            {task.kind === 'conversation_resume' && <>
                {task.spec.rule.kind === 'once' && <p>{formatTime(task.spec.rule.at, validTimezone(zone) ? zone : 'UTC', i18n.language)}</p>}
                <p>{t('schedules.proposal.resumeNote')}</p>
                {task.status === 'draft' && <Button disabled={!connected || busy} onClick={() => void activate()}>{t('schedules.activateResume')}</Button>}
            </>}
            {task.kind === 'fresh_task' && <>
                <div className="flex gap-2">
                    <Button variant={view === 'rehearsal' ? 'default' : 'outline'} onClick={() => setView('rehearsal')}>{t('schedules.rehearsal.view')}</Button>
                    <Button variant={view === 'contract' ? 'default' : 'outline'} onClick={() => setView('contract')}>{t('schedules.contract.title')}</Button>
                </div>
                {view === 'rehearsal' ? <RehearsalDetails client={client} scheduleId={scheduleId} connected={connected} zone={zone} assistantPaths={assistantPaths} />
                    : <ContractReview client={client} scheduleId={scheduleId} connected={connected} onPublished={value => { setTask(value); onChanged(); }} />}
            </>}
        </>}
    </div>;
}
