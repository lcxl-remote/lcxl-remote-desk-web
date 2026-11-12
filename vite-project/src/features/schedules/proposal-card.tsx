import { useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { AiAssistantToolActivity } from '@/features/desk/use-ai-assistant-chat';
import { useDeskSignaling } from '@/features/desk/use-desk-signaling';
import { ScheduleClient } from './client';
import { ProposalReview } from './proposal-review';

/** Inline decisions for fresh-task proposals that wait in the assistant turn. */
export function ScheduleProposalCards({ tools, onPendingCountChange }: { tools: AiAssistantToolActivity[]; running?: boolean; deviceId?: string; connectionId?: string; onPendingCountChange?: (count: number) => void }) {
    const { t } = useTranslation();
    const { isConnected, subscribe, sendTracked, cancelQueued } = useDeskSignaling();
    const client = useMemo(() => new ScheduleClient(sendTracked, cancelQueued), [sendTracked, cancelQueued]);
    const [decisions, setDecisions] = useState<Record<string, string>>({});
    const [ids, setIds] = useState<string[]>([]);
    const seen = useRef(new Set<string>());
    useEffect(() => {
        const unsubscribe = subscribe(client.receive);
        return () => { unsubscribe(); client.close(); };
    }, [client, subscribe]);
    useEffect(() => {
        const added: string[] = [];
        for (const tool of tools) {
            if (tool.name !== 'request_scheduled_task' || tool.status === 'failed' || !tool.output) continue;
            try {
                const value = JSON.parse(tool.output);
                if (value.awaiting_decision === true && value.kind === 'fresh_task'
                    && typeof value.schedule_id === 'string' && /^[a-f0-9-]{36}$/.test(value.schedule_id)
                    && !seen.current.has(value.schedule_id)) {
                    seen.current.add(value.schedule_id); added.push(value.schedule_id);
                }
            } catch { /* Non-proposal output stays in the ordinary activity view. */ }
        }
        if (added.length) setIds(previous => [...previous, ...added]);
    }, [tools]);
    useEffect(() => {
        onPendingCountChange?.(ids.filter(id => decisions[id] === 'pending_review').length);
    }, [decisions, ids, onPendingCountChange]);
    const decided = (status: string) => {
        if (status === 'active') return 'schedules.proposal.approved';
        if (status === 'deleted') return 'schedules.proposal.rejected';
        return null;
    };
    return <>
        {ids.map(id => {
            const outcome = decisions[id] ? decided(decisions[id]) : null;
            return <article key={id} data-assistant-pending={decisions[id] === 'pending_review' ? '' : undefined} tabIndex={-1}
                className="space-y-2 rounded-md border border-amber-500/40 p-3 empty:hidden">
                {outcome ? <p className="text-sm font-medium">{t(outcome)}</p>
                    : <ProposalReview client={client} scheduleId={id} connected={isConnected}
                        zone={Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC'} approvalCard
                        onLoaded={task => setDecisions(previous => ({ ...previous, [task.schedule_id]: task.status }))}
                        onChanged={task => setDecisions(previous => ({ ...previous, [task.schedule_id]: task.status }))} />}
            </article>;
        })}
    </>;
}
