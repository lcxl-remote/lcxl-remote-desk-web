import { useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription } from '@/components/ui/dialog';
import type { DeviceAssistantToolActivity } from '@/features/desk/use-device-assistant-chat';
import { useDeskSignaling } from '@/features/desk/use-desk-signaling';
import { ScheduleClient } from './client';
import { ProposalReview } from './proposal-review';

export function ScheduleProposalCards({ tools, running = false, deviceId, connectionId }: { tools: DeviceAssistantToolActivity[]; running?: boolean; deviceId: string; connectionId: string }) {
    const { t } = useTranslation();
    const { isConnected, subscribe, sendTracked, cancelQueued } = useDeskSignaling();
    const client = useMemo(() => new ScheduleClient(sendTracked, cancelQueued), [sendTracked, cancelQueued]);
    const [ids, setIds] = useState<string[]>([]);
    const [review, setReview] = useState<string | null>(null);
    const seen = useRef(new Set<string>());
    const queued = useRef<string[]>([]);
    useEffect(() => {
        const unsubscribe = subscribe(client.receive);
        return () => { unsubscribe(); client.close(); };
    }, [client, subscribe]);
    useEffect(() => {
        const added: string[] = [];
        for (const tool of tools) {
            if (tool.name !== 'request_scheduled_task' || tool.status !== 'ok' || !tool.output || tool.output.length > 8192) continue;
            try {
                const value = JSON.parse(tool.output);
                if (value.state === 'draft' && ['fresh_task', 'conversation_resume'].includes(value.kind)
                    && typeof value.schedule_id === 'string' && /^[a-f0-9-]{36}$/.test(value.schedule_id)
                    && !seen.current.has(value.schedule_id)) {
                    seen.current.add(value.schedule_id); added.push(value.schedule_id);
                }
            } catch { /* Non-proposal output stays in the ordinary activity view. */ }
        }
        if (added.length) { setIds(previous => [...previous, ...added]); queued.current.push(...added); }
        if (!review && !running && queued.current.length) setReview(queued.current.shift()!);
    }, [tools, running, review]);
    return <>
        {ids.map(id => <article key={id} className="space-y-2 rounded-lg border p-3">
            <p className="font-medium">{t('schedules.proposal.created')}</p>
            <p className="text-sm text-muted-foreground">{t('schedules.proposal.note')}</p>
            <Button variant="outline" onClick={() => setReview(id)}>{t('schedules.proposal.open')}</Button>
        </article>)}
        <Dialog open={review !== null} onOpenChange={open => { if (!open) setReview(null); }}>
            <DialogContent className="max-h-[85vh] overflow-y-auto sm:max-w-2xl">
                <DialogHeader><DialogTitle>{t('schedules.proposal.open')}</DialogTitle>
                    <DialogDescription>{t('schedules.proposal.note')}</DialogDescription></DialogHeader>
                {review && <ProposalReview client={client} scheduleId={review} connected={isConnected} activationDisabled={running}
                    zone={Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC'} assistantPaths={{ [deviceId]: `/desk/${encodeURIComponent(connectionId)}/assistant` }} onChanged={() => {}} />}
            </DialogContent>
        </Dialog>
    </>;
}
