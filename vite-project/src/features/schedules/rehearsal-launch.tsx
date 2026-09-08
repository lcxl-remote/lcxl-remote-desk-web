import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { v4 } from 'uuid';
import { Button } from '@/components/ui/button';
import type { ScheduleManagementRequest, ScheduleManagementResponse } from '@/services/types';
import { ScheduleClient, ScheduleRequestError } from './client';

type Details = Extract<ScheduleManagementResponse, { result: 'task_rehearsal' }>;
export function RehearsalLaunch({ details, client, path, connected, onReserved }: {
    details: Details; client: ScheduleClient; path?: string; connected: boolean;
    onReserved: (value: Details) => void;
}) {
    const { t } = useTranslation();
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState('');
    const alive = useRef(true);
    const pending = useRef(false);
    const [request] = useState<ScheduleManagementRequest>(() => ({ operation: 'reserve_rehearsal', schedule_id: details.task.schedule_id, expected_revision: details.task.revision, client_request_key: v4() }));
    useEffect(() => { alive.current = true; return () => { alive.current = false; }; }, []);
    const update = async (cancel: boolean) => {
        if (!connected || pending.current || (cancel ? !canCancel : !canReserve || !path)) return;
        const command: ScheduleManagementRequest = cancel
            ? { operation: 'cancel_pending_rehearsal', rehearsal_id: details.rehearsal!.rehearsal_id, expected_revision: details.task.revision }
            : request;
        pending.current = true; setBusy(true); setError('');
        try {
            const response = await client.request(command);
            if (!alive.current) return;
            if (response.result !== 'rehearsal' || response.task.schedule_id !== details.task.schedule_id || response.rehearsal.schedule_id !== details.task.schedule_id) throw new ScheduleRequestError('invalid');
            if (cancel && (response.rehearsal.rehearsal_id !== details.rehearsal?.rehearsal_id || response.rehearsal.status !== 'cancelled')) throw new ScheduleRequestError('invalid');
            onReserved({ result: 'task_rehearsal', task: response.task, rehearsal: response.rehearsal });
        } catch (err) {
            if (alive.current) setError(err instanceof ScheduleRequestError && err.reason === 'server' ? err.message : t('schedules.requestFailed'));
        } finally { pending.current = false; if (alive.current) setBusy(false); }
    };
    const rehearsal = details.rehearsal;
    const canReserve = details.task.kind === 'fresh_task' && !details.task.active_run_id && rehearsal?.status !== 'running'
        && ['draft', 'paused', 'awaiting_authorization'].includes(details.task.status)
        && !details.task.pause_reasons.includes('unknown_side_effect');
    const canCancel = rehearsal?.status === 'pending' && !rehearsal.started_at;
    const canOpen = rehearsal && (rehearsal.status === 'running' || rehearsal.status === 'completed'
        || (rehearsal.status === 'pending' && details.task.status === 'rehearsing'));
    const href = path && rehearsal ? `${import.meta.env.BASE_URL.replace(/\/$/, '')}${path}?rehearsal=${encodeURIComponent(rehearsal.rehearsal_id)}` : undefined;
    return <div className="space-y-2">
        {!path && <p>{t('schedules.rehearsal.deviceOffline')}</p>}
        {canOpen && href && connected && <Button asChild><a href={href}>{t('schedules.rehearsal.open')}</a></Button>}
        {canReserve && <Button disabled={!connected || !path || busy} onClick={() => void update(false)}>{t(busy ? 'schedules.saving' : 'schedules.rehearsal.prepare')}</Button>}
        {canCancel && <Button variant="outline" disabled={!connected || busy} onClick={() => void update(true)}>{t(busy ? 'schedules.saving' : 'schedules.rehearsal.cancelPending')}</Button>}
        {canReserve && <p className="text-sm text-muted-foreground">{t('schedules.rehearsal.prepareNote')}</p>}
        {error && <p role="alert">{error}</p>}
    </div>;
}
