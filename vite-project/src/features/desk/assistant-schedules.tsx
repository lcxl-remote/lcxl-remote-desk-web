import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription } from '@/components/ui/dialog';
import type { ScheduleView } from '@/services/types';
import { useDeskSignaling } from './use-desk-signaling';
import { ScheduleClient } from '@/features/schedules/client';

export function AssistantSchedules({ open, onOpenChange, deviceId, sessionId }: {
    open: boolean; onOpenChange: (open: boolean) => void; deviceId: string; sessionId: string | null;
}) {
    // Schedule sources use the server session ID, not the client conversation UUID.
    const { t } = useTranslation();
    const { isConnected, subscribe, sendTracked, cancelQueued } = useDeskSignaling();
    const client = useMemo(() => new ScheduleClient(sendTracked, cancelQueued), [sendTracked, cancelQueued]);
    const [tasks, setTasks] = useState<ScheduleView[]>([]);
    const [error, setError] = useState(false);
    const [busy, setBusy] = useState(false);
    const [refresh, setRefresh] = useState(0);
    useEffect(() => {
        const unsubscribe = subscribe(client.receive);
        return () => { unsubscribe(); client.close(); };
    }, [client, subscribe]);
    useEffect(() => {
        if (!open || !isConnected || !sessionId) return;
        let active = true;
        let timer: ReturnType<typeof setTimeout>;
        const load = async () => {
            setBusy(true); setError(false);
            try {
                const rows: ScheduleView[] = [];
                let after: string | null = null;
                do {
                    const response = await client.request({ operation: 'search', kind: 'conversation_resume',
                        target_device_id: deviceId, source_conversation_id: sessionId, attention_only: false, limit: 50, after });
                    if (!active) return;
                    if (response.result !== 'search_results') throw new Error('invalid');
                    rows.push(...response.tasks);
                    after = response.next_cursor ?? null;
                } while (after);
                setTasks(rows);
            } catch { if (active) setError(true); }
            finally {
                if (active) { setBusy(false); timer = setTimeout(load, 5000); }
            }
        };
        setTasks([]);
        void load();
        return () => { active = false; clearTimeout(timer); client.close(); };
    }, [open, isConnected, deviceId, sessionId, client, refresh]);
    return <Dialog open={open} onOpenChange={onOpenChange}>
        <DialogContent className="flex max-h-[85dvh] flex-col overflow-hidden sm:max-w-xl">
            <DialogHeader><DialogTitle>{t('pages.deviceAssistant.schedules.title')}</DialogTitle>
                <DialogDescription>{t('pages.deviceAssistant.schedules.hint')}</DialogDescription></DialogHeader>
            <div className="min-h-0 space-y-3 overflow-y-auto">
                {!isConnected && <p role="status">{t('schedules.connecting')}</p>}
                {error && <p role="alert">{t('pages.deviceAssistant.schedules.loadError')}</p>}
                {busy && tasks.length === 0 && <p role="status">{t('schedules.loading')}</p>}
                {!busy && !error && isConnected && tasks.length === 0 && <p>{t('pages.deviceAssistant.schedules.empty')}</p>}
                {tasks.map(task => <article key={task.schedule_id} className="space-y-2 rounded-lg border p-3">
                    <div className="flex flex-wrap justify-between gap-2"><h3 className="break-words font-medium">{task.title}</h3>
                        <span className="text-sm text-muted-foreground">{t(`schedules.status.${task.status}`)}</span></div>
                    <p className="whitespace-pre-wrap break-words text-sm">{task.prompt}</p>
                    {task.next_run_at && <p className="text-sm text-muted-foreground">{new Date(task.next_run_at).toLocaleString()}</p>}
                </article>)}
            </div>
            <Button type="button" variant="outline" disabled={busy || !isConnected || !sessionId} onClick={() => setRefresh(value => value + 1)}>{t('schedules.refresh')}</Button>
        </DialogContent>
    </Dialog>;
}
