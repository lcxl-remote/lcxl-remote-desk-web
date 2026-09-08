import { RehearsalLaunch } from './rehearsal-launch';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import type { ScheduleManagementResponse } from '@/services/types';
import { ScheduleClient, ScheduleRequestError } from './client';
import { formatTime, validTimezone } from './time';

type Details = Extract<ScheduleManagementResponse, { result: 'task_rehearsal' }>;
type Permissions = Extract<ScheduleManagementResponse, { result: 'rehearsal_permissions' }>;

export function RehearsalDetails({ client, scheduleId, connected, zone, assistantPaths = {} }: {
    client: ScheduleClient; scheduleId: string; connected: boolean; zone: string; assistantPaths?: Record<string, string>;
}) {
    const { t, i18n } = useTranslation();
    const [details, setDetails] = useState<Details | null>(null);
    const [permissions, setPermissions] = useState<Permissions | null>(null);
    const [loading, setLoading] = useState(false);
    const [error, setError] = useState('');
    const [refresh, setRefresh] = useState(0);
    useEffect(() => {
        let active = true;
        setDetails(null); setPermissions(null); setError(''); setLoading(connected);
        if (!connected) return () => { active = false; };
        void (async () => {
            try {
                const response = await client.request({ operation: 'get_task_rehearsal', schedule_id: scheduleId });
                if (!active) return;
                if (response.result !== 'task_rehearsal' || response.task.schedule_id !== scheduleId || (response.rehearsal && response.rehearsal.schedule_id !== scheduleId)) throw new ScheduleRequestError('invalid');
                setDetails(response);
                if (response.rehearsal?.status === 'completed') {
                    const report = await client.request({ operation: 'get_rehearsal_permissions', rehearsal_id: response.rehearsal.rehearsal_id });
                    if (!active) return;
                    if (report.result !== 'rehearsal_permissions' || report.rehearsal_id !== response.rehearsal.rehearsal_id) throw new ScheduleRequestError('invalid');
                    setPermissions(report);
                }
            } catch (err) {
                if (active) setError(err instanceof ScheduleRequestError && err.reason === 'server' ? err.message : t('schedules.requestFailed'));
            } finally { if (active) setLoading(false); }
        })();
        return () => { active = false; };
    }, [client, scheduleId, connected, refresh, t]);
    const rehearsal = details?.rehearsal;
    const displayZone = validTimezone(zone) ? zone : 'UTC';
    return <div className="space-y-4">
        <Button variant="outline" disabled={!connected || loading} onClick={() => setRefresh(value => value + 1)}>{t('schedules.refresh')}</Button>
        {!connected && <p role="status">{t('schedules.connecting')}</p>}
        {loading && <p role="status">{t('schedules.loading')}</p>}
        {error && <p role="alert">{error}</p>}
        {details && <RehearsalLaunch key={`${details.task.schedule_id}:${details.task.revision}`} details={details} client={client} connected={connected}
            path={assistantPaths[rehearsal?.target_device_id ?? details.task.target_device_id ?? '']}
            onReserved={value => { setDetails(value); setPermissions(null); }} />}
        {details && !rehearsal && <p>{t('schedules.rehearsal.empty')}</p>}
        {rehearsal && <>
            <p>{t(`schedules.rehearsal.status.${rehearsal.status}`)}</p>
            <p className="whitespace-pre-wrap">{rehearsal.prompt}</p>
            {rehearsal.started_at && <p>{t('schedules.rehearsal.started')}: {formatTime(rehearsal.started_at, displayZone, i18n.language)}</p>}
            {rehearsal.finished_at && <p>{t('schedules.rehearsal.finished')}: {formatTime(rehearsal.finished_at, displayZone, i18n.language)}</p>}
        </>}
        {permissions && <>
            <p className="text-sm text-muted-foreground">{t('schedules.rehearsal.historyNote')}</p>
            {!permissions.observations.length && <p>{t('schedules.rehearsal.noVerified')}</p>}
            {permissions.observations.map(item => <article key={item.tool_call_id} className="space-y-2 rounded-md border p-3 text-sm">
                <h3 className="font-semibold break-all">{item.tool_name}</h3>
                <p>{t(`schedules.rehearsal.approval.${item.approval_source}`)}</p>
                <p className="break-all">{t('schedules.rehearsal.resources')}: {item.resources.join(' · ')}</p>
                <p className="break-all">{t('schedules.rehearsal.operations')}: {item.operations.join(' · ')}</p>
                {!!item.export_destinations.length && <p className="break-all">{t('schedules.rehearsal.destinations')}: {item.export_destinations.map(destination => Object.values(destination).filter(value => typeof value === 'string').join(' · ')).join('; ')}</p>}
                <p>{formatTime(item.completed_at, displayZone, i18n.language)}</p>
            </article>)}
            {!!permissions.unconfirmed_tool_call_ids.length && <p role="status">{t('schedules.rehearsal.unconfirmed', { count: permissions.unconfirmed_tool_call_ids.length })}</p>}
            {!!permissions.unclassified_tool_call_ids.length && <p role="status">{t('schedules.rehearsal.unclassified', { count: permissions.unclassified_tool_call_ids.length })}</p>}
        </>}
    </div>;
}
