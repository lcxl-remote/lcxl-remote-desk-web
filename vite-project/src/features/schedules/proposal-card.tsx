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
    const [decisions, setDecisions] = useState<Record<string, string>>({});
    const [ids, setIds] = useState<string[]>([]);
    const [review, setReview] = useState<string | null>(null);
    const seen = useRef(new Set<string>());
    const queued = useRef<string[]>([]);
    const waiting = useRef(new Set<string>());
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
                if (((value.state === 'draft' && value.kind === 'fresh_task') || (value.state === 'pending_review' && value.kind === 'conversation_resume'))
                    && typeof value.schedule_id === 'string' && /^[a-f0-9-]{36}$/.test(value.schedule_id)
                    && !seen.current.has(value.schedule_id)) {
                    seen.current.add(value.schedule_id); added.push(value.schedule_id);
                    if (value.kind === 'conversation_resume') waiting.current.add(value.schedule_id);
                }
            } catch { /* Non-proposal output stays in the ordinary activity view. */ }
        }
        if (added.length) { setIds(previous => [...previous, ...added]); queued.current.push(...added.filter(id => !waiting.current.has(id))); }
        if (!running && !review && queued.current.length) setReview(queued.current.shift()!);
    }, [tools, running, review]);
    return <>
        {ids.map(id => <article key={id} className="space-y-2 rounded-md border border-amber-500/40 p-3">
            <p className="text-sm font-medium">{t(decisions[id] === 'active' ? 'schedules.proposal.approved' : decisions[id] === 'deleted' ? 'schedules.proposal.rejected' : 'schedules.proposal.created')}</p>
            <p className="text-xs text-muted-foreground">{t('schedules.proposal.note')}</p>
            {waiting.current.has(id) ? <ProposalReview client={client} scheduleId={id} connected={isConnected}
                zone={Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC'} assistantPaths={{}} approvalCard
                onChanged={task => setDecisions(previous => ({ ...previous, [task.schedule_id]: task.status }))} />
                : <Button type="button" size="sm" variant="outline" onClick={() => setReview(id)}>{t('schedules.proposal.open')}</Button>}
        </article>)}
        <Dialog open={review !== null} onOpenChange={open => { if (!open) setReview(null); }}>
            <DialogContent className="flex max-h-[85vh] flex-col overflow-hidden sm:max-w-xl">
                <DialogHeader className="shrink-0 text-left"><DialogTitle>{t('schedules.proposal.open')}</DialogTitle>
                    <DialogDescription>{t('schedules.proposal.reviewHint')}</DialogDescription></DialogHeader>
                <div className="min-h-0 overflow-y-auto">
                    {review && <ProposalReview client={client} scheduleId={review} connected={isConnected} activationDisabled={running}
                        zone={Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC'} assistantPaths={{ [deviceId]: `/desk/${encodeURIComponent(connectionId)}/assistant` }} approvalCard onChanged={task => { setDecisions(previous => ({ ...previous, [task.schedule_id]: task.status })); setReview(null); }} />}
                </div>
            </DialogContent>
        </Dialog>
    </>;
}
