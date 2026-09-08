import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { v4 } from 'uuid';
import { Button } from '@/components/ui/button';
import type { ScheduleClient } from './client';

export function OutcomeReview({ client, scheduleId, runId, connected, onReload }: {
    client: ScheduleClient; scheduleId: string; runId: string; connected: boolean; onReload: () => Promise<void>;
}) {
    const { t } = useTranslation();
    const [note, setNote] = useState('');
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState(false);
    const inFlight = useRef(false);
    const epoch = useRef(0);
    useEffect(() => { ++epoch.current; inFlight.current = false; setBusy(false); setError(false); return () => { ++epoch.current; }; }, [scheduleId, runId, connected]);
    const submit = async () => {
        if (!connected || inFlight.current || !note.trim() || new TextEncoder().encode(note).length > 2048) return;
        const current = epoch.current;
        inFlight.current = true; setBusy(true); setError(false);
        try {
            const task = await client.request({ operation: 'get', schedule_id: scheduleId });
            if (current !== epoch.current) return;
            if (task.result !== 'task' || task.task.status !== 'paused') throw new Error('not paused');
            await client.request({ operation: 'acknowledge_run_outcome', schedule_id: scheduleId, run_id: runId,
                expected_revision: task.task.revision, client_request_key: v4(), note });
        } catch { if (current === epoch.current) setError(true); }
        finally {
            if (current === epoch.current) {
                try { await onReload(); }
                finally { if (current === epoch.current) { inFlight.current = false; setBusy(false); } }
            }
        }
    };
    return <div className="space-y-2" aria-busy={busy}>
        <p className="text-sm">{t('schedules.review.note')}</p>
        <label className="block text-sm">{t('schedules.review.description')}
            <textarea className="mt-1 block w-full rounded border bg-background p-2" value={note}
                disabled={!connected || busy} maxLength={2048} onChange={event => setNote(event.target.value)} />
        </label>
        {error && <p role="alert">{t('schedules.requestFailed')}</p>}
        <Button disabled={!connected || busy || !note.trim() || new TextEncoder().encode(note).length > 2048}
            onClick={() => void submit()}>{t('schedules.review.confirm')}</Button>
    </div>;
}
