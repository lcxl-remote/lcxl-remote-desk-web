import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { v4 } from 'uuid';
import { Button } from '@/components/ui/button';
import type { DeviceAssistantSessionSnapshotDto } from '@/services/types';
import type { ScheduleClient } from './client';

export function RunDirectories({ client, scheduleId, runId, snapshot, connected, loading, onReload }: {
    client: Pick<ScheduleClient, 'request'>; scheduleId: string; runId: string;
    snapshot: DeviceAssistantSessionSnapshotDto; connected: boolean; loading: boolean;
    onReload: () => Promise<void>;
}) {
    const { t } = useTranslation();
    const [busy, setBusy] = useState(false);
    const [notice, setNotice] = useState<'recorded' | 'rejected' | null>(null);
    const epoch = useRef(0);
    const inFlight = useRef(false);
    useEffect(() => {
        ++epoch.current; inFlight.current = false; setBusy(false); setNotice(null);
        return () => { ++epoch.current; };
    }, [scheduleId, runId, snapshot.sessionId, connected]);
    const scope = snapshot.fileScope;
    const canDecide = connected && !loading && !busy && snapshot.requestId === runId;
    const decide = async (requestId: string, approve: boolean | null) => {
        const directory = scope.directories.find(item => item.requestId === requestId);
        if (!canDecide || inFlight.current || !directory) return;
        if (approve === null ? directory.state !== 'approved' : directory.state !== 'pending') return;
        if (approve && !(Date.parse(directory.referenceExpiresAt) > Date.now())) return;
        const current = epoch.current;
        inFlight.current = true; setBusy(true); setNotice(null);
        let recorded = false;
        try {
            const subject = { schedule_id: scheduleId, run_id: runId, directory_request_id: requestId,
                expected_scope_revision: scope.revision, client_request_key: v4() };
            const result = await client.request(approve === null
                ? { ...subject, operation: 'revoke_run_directory' }
                : { ...subject, operation: 'decide_run_directory', approve });
            recorded = result.result === 'task' && result.task.schedule_id === scheduleId;
        } catch { /* An uncertain response is re-read, never automatically resubmitted. */ }
        if (current !== epoch.current) return;
        setNotice(recorded ? 'recorded' : 'rejected');
        try { await onReload(); }
        catch { if (current === epoch.current) setNotice('rejected'); }
        finally { if (current === epoch.current) { inFlight.current = false; setBusy(false); } }
    };
    return <section className="space-y-3" aria-busy={busy} aria-label={t('pages.deviceAssistant.directories.title')}>
        <h3 className="font-semibold">{t('pages.deviceAssistant.directories.title')}</h3>
        <p className="text-sm text-muted-foreground">{t('pages.deviceAssistant.directories.hint')}</p>
        {notice && <p role={notice === 'rejected' ? 'alert' : 'status'}>{t(`schedules.approval.${notice}`)}</p>}
        {scope.directories.map(directory => <article key={directory.requestId} className="space-y-2 rounded-md border p-3">
            <p className="break-all font-mono text-sm">{directory.canonicalPath}</p>
            <p className="break-words text-sm">{directory.purpose}</p>
            <p className="text-sm">{t(`schedules.directoryState.${directory.state}`)}</p>
            <p className="text-xs">{t('pages.deviceAssistant.directories.expiry', { time: new Date(directory.referenceExpiresAt).toLocaleString() })}</p>
            {directory.state === 'approved' && <Button variant="outline" disabled={!canDecide} onClick={() => void decide(directory.requestId, null)}>{t('pages.deviceAssistant.directories.remove')}</Button>}
            {directory.state === 'pending' && <div className="flex gap-2">
                <Button disabled={!canDecide || !(Date.parse(directory.referenceExpiresAt) > Date.now())} onClick={() => void decide(directory.requestId, true)}>{t('pages.deviceAssistant.directories.approve')}</Button>
                <Button variant="outline" disabled={!canDecide} onClick={() => void decide(directory.requestId, false)}>{t('pages.deviceAssistant.directories.reject')}</Button>
            </div>}
        </article>)}
    </section>;
}
