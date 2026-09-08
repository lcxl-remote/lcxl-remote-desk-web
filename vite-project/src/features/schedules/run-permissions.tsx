import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { AssistantPermissionRequest } from '@/features/desk/assistant-permission-request';
import { deskErrorCodeEnum, type DeviceAssistantSessionSnapshotDto, type PermissionDecisionBody, type PermissionRequestDto, type ScheduleView } from '@/services/types';
import type { ScheduleClient } from './client';

type ApprovalSnapshot = Pick<DeviceAssistantSessionSnapshotDto, 'sessionId' | 'seq' | 'requestId' | 'inputRevision' | 'permissionRequests'>;

export function RunPermissions({ client, scheduleId, runId, snapshot, connectionIds = {}, connected, loading, onReload }: {
    client: Pick<ScheduleClient, 'request'>; scheduleId: string; runId: string; snapshot: ApprovalSnapshot;
    connectionIds?: Record<string, string>; connected: boolean; loading: boolean; onReload: () => Promise<void>;
}) {
    const { t } = useTranslation();
    const [task, setTask] = useState<ScheduleView | null>(null);
    const [validatedSnapshot, setValidatedSnapshot] = useState<ApprovalSnapshot | null>(null);
    const [submitting, setSubmitting] = useState(false);
    const [notice, setNotice] = useState<'recorded' | 'rejected' | null>(null);
    const inFlight = useRef(false);
    const mutationEpoch = useRef(0);
    const abort = useRef<AbortController | null>(null);
    const connection = task?.target_device_id && Object.prototype.hasOwnProperty.call(connectionIds, task.target_device_id)
        ? connectionIds[task.target_device_id] : undefined;
    useEffect(() => {
        let active = true;
        setValidatedSnapshot(null);
        if (!connected) setTask(null);
        if (connected) void client.request({ operation: 'get', schedule_id: scheduleId }).then(response => {
            if (active && response.result === 'task' && response.task.schedule_id === scheduleId) {
                setTask(response.task); setValidatedSnapshot(snapshot);
            }
        }).catch(() => { /* Keep approval disabled when current task metadata is unavailable. */ });
        return () => { active = false; };
    }, [client, scheduleId, connected, snapshot]);
    useEffect(() => {
        ++mutationEpoch.current; abort.current?.abort(); inFlight.current = false;
        setSubmitting(false); setNotice(null);
        return () => { ++mutationEpoch.current; abort.current?.abort(); };
    }, [scheduleId, runId, snapshot.sessionId, connected, connection]);
    const currentRun = snapshot.requestId === runId && task?.active_run_id === runId
        && ['active', 'triggered', 'paused'].includes(task.status);
    const hasCurrentPending = snapshot.permissionRequests.some(request => request.state === 'pending' && request.inputRevision === snapshot.inputRevision);
    const canDecide = hasCurrentPending && connected && !!connection && currentRun && !loading && validatedSnapshot === snapshot;
    const decide = async (request: PermissionRequestDto, items: PermissionDecisionBody['items']) => {
        if (!canDecide || inFlight.current || request.state !== 'pending' || request.inputRevision !== snapshot.inputRevision
            || !snapshot.permissionRequests.includes(request)) return false;
        inFlight.current = true; setSubmitting(true); setNotice(null);
        const epoch = mutationEpoch.current;
        const controller = new AbortController(); abort.current = controller;
        const body: PermissionDecisionBody = {
            connection: connection!, session: snapshot.sessionId, requestId: request.requestId,
            expectedRunRequestId: runId, items,
        };
        let recorded = false;
        try {
            const response = await fetch('/api/my/device-assistant-session/permission-decision', {
                method: 'POST', credentials: 'include', headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
                body: JSON.stringify(body), signal: controller.signal,
            });
            const result = response.ok ? await response.json() : null;
            recorded = result?.code === deskErrorCodeEnum.SUCCESS && ['approved', 'partially_approved', 'denied'].includes(result?.data?.state);
        } catch { /* Re-read the durable decision after rejection or an uncertain response; never retry automatically. */ }
        if (mutationEpoch.current !== epoch) return false;
        setNotice(recorded ? 'recorded' : 'rejected');
        // A submitted decision is not evidence that the scheduled execution succeeded.
        try { await onReload(); }
        catch { if (mutationEpoch.current === epoch) setNotice('rejected'); }
        finally {
            if (mutationEpoch.current === epoch) { inFlight.current = false; setSubmitting(false); }
        }
        return recorded;
    };
    return <section className="space-y-3" aria-label={t('schedules.approval.title')} aria-busy={submitting}>
        <h3 className="font-semibold">{t('schedules.approval.title')}</h3>
        <p className="text-sm text-muted-foreground">{t('schedules.approval.note')}</p>
        {!canDecide && snapshot.permissionRequests.some(request => request.state === 'pending') &&
            <p role="status">{t('schedules.approval.unavailable')}</p>}
        {notice && <p role={notice === 'rejected' ? 'alert' : 'status'}>{t(`schedules.approval.${notice}`)}</p>}
        {snapshot.permissionRequests.map(request => <AssistantPermissionRequest
            key={`${snapshot.sessionId}:${request.requestId}:${request.inputRevision}`} request={request}
            canDecide={currentRun && request.inputRevision === snapshot.inputRevision}
            disabled={!canDecide} busy={submitting} onDecide={decide} />)}
    </section>;
}
