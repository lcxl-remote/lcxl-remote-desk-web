import { OutcomeReview } from './outcome-review';
import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import type { ScheduledRunView } from '@/services/types';
import { RunResult } from './run-result';
import { ScheduleClient, ScheduleRequestError } from './client';
import { formatTime, validTimezone } from './time';

export function RunHistory({ client, scheduleId, connected, zone, selectedRun, onSelectRun, connectionIds }: {
    client: ScheduleClient; scheduleId: string; connected: boolean; zone: string;
    connectionIds?: Record<string, string>;
    selectedRun?: string | null; onSelectRun?: (run: string | null) => void;
}) {
    const { t, i18n } = useTranslation();
    const [localSelected, setLocalSelected] = useState<string | null>(null);
    const selected = selectedRun === undefined ? localSelected : selectedRun;
    const selectRun = onSelectRun ?? setLocalSelected;
    const [runs, setRuns] = useState<ScheduledRunView[]>([]);
    const [cursor, setCursor] = useState<string | null>(null);
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState(false);
    const generation = useRef(0);
    const load = useCallback(async (before: string | null = null) => {
        const current = ++generation.current;
        setBusy(true); setError(false);
        try {
            const response = await client.request({ operation: 'list_runs', schedule_id: scheduleId, before, limit: 25 });
            if (current !== generation.current) return;
            if (response.result !== 'runs' || response.schedule_id !== scheduleId) throw new ScheduleRequestError('invalid');
            setRuns(previous => before ? [...previous.filter(row => !response.runs.some(next => next.run_id === row.run_id)), ...response.runs] : response.runs);
            setCursor(response.next_cursor ?? null);
        } catch { if (current === generation.current) setError(true); }
        finally { if (current === generation.current) setBusy(false); }
    }, [client, scheduleId]);
    const cancelRun = async (runId: string) => {
        if (!connected || busy) return;
        const current = ++generation.current;
        setBusy(true); setError(false);
        let failed = false;
        try {
            await client.request({ operation: 'cancel_task_run', run_id: runId });
        } catch { failed = true; }
        if (current === generation.current) {
            await load();
            if (failed && current + 1 === generation.current) setError(true);
        }
    };
    useEffect(() => {
        setRuns([]); setCursor(null); setError(false); setBusy(false);
        if (connected) void load();
        return () => { ++generation.current; };
    }, [connected, load]);
    if (selected) return <RunResult key={`${scheduleId}:${selected}`} scheduleId={scheduleId} runId={selected} client={client} connectionIds={connectionIds} connected={connected} onBack={() => selectRun(null)} />;
    const displayZone = validTimezone(zone) ? zone : 'UTC';
    const time = (value: string) => formatTime(value, displayZone, i18n.language);
    return <div className="space-y-3" aria-busy={busy}>
        <Button variant="outline" disabled={!connected || busy} onClick={() => void load()}>{t('schedules.refresh')}</Button>
        {!connected && <p role="status">{t('schedules.connecting')}</p>}
        {error && <p role="alert">{t('schedules.requestFailed')}</p>}
        {connected && !busy && !error && !runs.length && <p>{t('schedules.history.empty')}</p>}
        {busy && <p role="status">{t('schedules.loading')}</p>}
        {runs.map(run => <article key={run.run_id} className="space-y-1 rounded-lg border p-3">
            <p className="font-medium">{t(`schedules.runStatus.${run.status}`)}</p>
            <p>{t(`schedules.runSource.${run.source}`)}</p>
            {run.issue && <p>{t('schedules.history.reason')}: {t(run.issue.kind === 'agent' ? `schedules.runIssue.agent.${run.issue.error}` : `schedules.runIssue.${run.issue.kind}`)}</p>}
            <p>{t('schedules.history.requested')}: {time(run.requested_at)}</p>
            {run.scheduled_at && <p>{t('schedules.history.scheduled')}: {time(run.scheduled_at)}</p>}
            {run.started_at && <p>{t('schedules.history.started')}: {time(run.started_at)}</p>}
            {run.finished_at && <p>{t('schedules.history.finished')}: {time(run.finished_at)}</p>}
            {run.receipts_reconciled_at && <p>{t('schedules.history.receiptsReconciled', { time: time(run.receipts_reconciled_at) })}</p>}
            {run.outcome_reviewed_at && <p>{t('schedules.review.reviewed', { time: time(run.outcome_reviewed_at) })}</p>}
            {!run.outcome_reviewed_at && run.status === 'outcome_unknown' &&
                <OutcomeReview client={client} scheduleId={scheduleId} runId={run.run_id} connected={connected} onReload={() => load()} />}
            {run.cancel_requested_at && <p>{t('schedules.history.cancelRequested')}: {time(run.cancel_requested_at)}</p>}
            {['queued', 'waiting_device', 'running', 'awaiting_permission'].includes(run.status) &&
                <Button variant="outline" disabled={!connected || busy || !!run.cancel_requested_at}
                    onClick={() => void cancelRun(run.run_id)}>{t('schedules.cancelRun')}</Button>}
            {run.started_at && <Button variant="outline" disabled={!connected} onClick={() => selectRun(run.run_id)}>{t('schedules.result.open')}</Button>}
            {run.missed_count > 0 && <p>{t('schedules.history.missed', { count: run.missed_count })}</p>}
        </article>)}
        {cursor && <Button variant="outline" disabled={!connected || busy} onClick={() => void load(cursor)}>{t('schedules.history.more')}</Button>}
    </div>;
}
