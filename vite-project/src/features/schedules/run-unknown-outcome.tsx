import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { v4 } from 'uuid';
import { type DeviceAssistantSessionSnapshotDto } from '@/services/types';
import type { ScheduleClient } from './client';

export function RunUnknownOutcome({ client, scheduleId, runId, snapshot, connected, loading, onReload }: {
    client: Pick<ScheduleClient, 'request'>; scheduleId: string; runId: string;
    snapshot: DeviceAssistantSessionSnapshotDto;
    connected: boolean; loading: boolean; onReload: () => Promise<void>;
}) {
    const { t } = useTranslation();
    const [checked, setChecked] = useState(false);
    const [note, setNote] = useState('');
    const [busy, setBusy] = useState(false);
    const [failed, setFailed] = useState(false);
    const epoch = useRef(0);
    const inFlight = useRef(false);
    useEffect(() => {
        ++epoch.current; inFlight.current = false; setBusy(false); setFailed(false); setChecked(false);
        return () => { ++epoch.current; };
    }, [scheduleId, runId, snapshot, connected]);
    const dispose = async () => {
        const outcome = snapshot.unresolvedOutcome;
        if (!outcome || !connected || loading || !checked || !note.trim() || new TextEncoder().encode(note).length > 2048 || inFlight.current) return;
        const current = epoch.current;
        inFlight.current = true; setBusy(true); setFailed(false);
        try {
            const result = await client.request({ operation: 'get', schedule_id: scheduleId });
            if (current !== epoch.current) return;
            if (result.result !== 'task' || result.task.schedule_id !== scheduleId
                || result.task.kind !== 'fresh_task' || result.task.status !== 'paused'
                || result.task.active_run_id || snapshot.requestId !== runId) throw new Error('not settled');
            await client.request({ operation: 'dispose_run_outcome', schedule_id: scheduleId,
                run_id: runId, expected_revision: result.task.revision, client_request_key: v4(),
                work_id: outcome.workId, execution_id: outcome.executionId, note });
        } catch { if (current === epoch.current) setFailed(true); }
        finally {
            if (current === epoch.current) {
                try { await onReload(); }
                catch { if (current === epoch.current) setFailed(true); }
                finally { if (current === epoch.current) { inFlight.current = false; setBusy(false); } }
            }
        }
    };
    if (!snapshot.unresolvedOutcome) return null;
    return <section className="space-y-2 rounded-lg border p-3" aria-busy={busy}>
        <p>{t('schedules.unknown.note')}</p>
        <label className="flex items-start gap-2 text-sm">
            <input type="checkbox" checked={checked} disabled={!connected || busy || loading}
                onChange={event => setChecked(event.target.checked)} />
            {t('schedules.unknown.checked')}
        </label>
        <label className="block text-sm">{t('schedules.review.description')}
            <textarea className="mt-1 block w-full rounded border bg-background p-2" value={note}
                disabled={!connected || busy || loading} maxLength={2048} onChange={event => setNote(event.target.value)} />
        </label>
        {failed && <p role="alert">{t('schedules.unknown.failed')}</p>}
        <Button disabled={!connected || loading || busy || !checked || !note.trim() || new TextEncoder().encode(note).length > 2048} onClick={() => void dispose()}>{t('schedules.unknown.dispose')}</Button>
    </section>;
}
