import { useEffect, useRef, useState } from 'react';
import { useInfiniteQuery, useQuery, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { Bot, FileSearch, ArrowLeft, RefreshCw } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Checkbox } from '@/components/ui/checkbox';
import { Label } from '@/components/ui/label';
import { useGetCurrentUser } from '@/services/hooks/userController/useGetCurrentUser';
import type { ApprovalReviewDto, ApprovalTracePartDto } from '@/services/types';
import { approvalApi, approvalAccessError, type ApprovalApi } from './api';

export function useOwnApprovalAccess() {
    const user = useGetCurrentUser().data?.data;
    return { allowed: user?.access === 'admin' && user?.target_connection_id == null,
        identity: `${user?.user_id ?? user?.name ?? ''}:${user?.access ?? ''}:${user?.target_connection_id ?? ''}` };
}
function time(value?: number | null) { return value ? new Date(value).toLocaleString() : '—'; }
export function groupApprovalParts(parts: ApprovalTracePartDto[]): [number, ApprovalTracePartDto[]][] {
    const groups = new Map<number, ApprovalTracePartDto[]>();
    for (const part of parts) { const group = groups.get(part.attempt_index) ?? []; group.push(part); groups.set(part.attempt_index, group); }
    return [...groups.entries()].sort(([left], [right]) => left - right);
}
function AttemptParts({ parts, kind, readable, complete }: { parts: ApprovalTracePartDto[]; kind: string; readable: boolean; complete: boolean }) {
    const { t } = useTranslation();
    let input: { role?: string; content?: unknown }[] | null = null;
    if (kind === 'input' && readable && complete && parts.reduce((sum, part) => sum + part.text.length, 0) <= 262144) {
        try {
            const body: unknown = JSON.parse(parts.map(part => part.text).join(''));
            if (body && typeof body === 'object' && 'messages' in body && Array.isArray(body.messages)) {
                input = body.messages;
                if ('system' in body) input = [{ role: 'system', content: body.system }, ...input!];
            }
        } catch { /* Incomplete protocol chunks remain literal text. */ }
    }
    if (input) return <>{input.map((message, index) => <Card key={index}><CardHeader className="py-3"><CardTitle className="text-sm">{t(`approval.role.${message.role ?? 'unknown'}`, { defaultValue: t('approval.unknown') })}</CardTitle></CardHeader><CardContent><pre className="whitespace-pre-wrap break-words text-xs">{typeof message.content === 'string' ? message.content : JSON.stringify(message.content, null, 2)}</pre></CardContent></Card>)}</>;
    return <>{parts.map(part => <pre key={part.sequence} className="max-w-full whitespace-pre-wrap break-words rounded border bg-muted/30 p-3 text-xs">{kind === 'event' ? part.text.split('\n').filter(Boolean).map(line => {
        try { const event = JSON.parse(line) as { stage: string; origin?: string; at_ms: number }; return `${time(event.at_ms)} · ${t(`approval.origin.${event.origin ?? 'server'}`)} · ${t(`approval.stage.${event.stage}`, { defaultValue: t('approval.unknown') })}`; } catch { return t('approval.recording.partial'); }
    }).join('\n') : part.text}</pre>)}</>;
}
function ReviewParts({ session, record, identity, api, onAccessLost, kind }: { session: string; record: ApprovalReviewDto; identity: string; api: ApprovalApi; onAccessLost: () => void; kind: string }) {
    const { t } = useTranslation();
    const [readable, setReadable] = useState(true);
    const client = useQueryClient();
    const bodyAvailable = !['expired', 'budget_cleaned', 'deleted', 'disabled', 'not_sent', 'not_recorded'].includes(record.recording_state);
    const query = useInfiniteQuery({
        queryKey: ['approval-own', location.origin, identity, session, record.id, 'parts', kind],
        enabled: bodyAvailable, refetchInterval: record.recording_state === 'collecting' ? 3000 : false,
        initialPageParam: undefined as number | undefined,
        queryFn: ({ signal, pageParam }) => api.parts({ session, id: record.id, kind, after: pageParam }, signal),
        getNextPageParam: last => last.next_after ?? undefined, retry: false, gcTime: 0,
    });
    useEffect(() => { if (approvalAccessError(query.error)) onAccessLost(); }, [query.error, onAccessLost]);
    useEffect(() => { if (!bodyAvailable) client.removeQueries({ queryKey: ['approval-own', location.origin, identity, session, record.id, 'parts', kind] }); }, [bodyAvailable, client, identity, session, record.id, kind]);
    const lastState = useRef(record.recording_state);
    useEffect(() => { if (bodyAvailable && lastState.current === 'collecting' && record.recording_state !== 'collecting') void query.refetch(); lastState.current = record.recording_state; }, [record.recording_state, bodyAvailable]);
    const parts = bodyAvailable ? query.data?.pages.flatMap(page => page.parts) ?? [] : [];
    if (approvalAccessError(query.error)) return <p role="alert">{t('approval.noAccess')}</p>;
    return <div className="space-y-3">
        {kind === 'input' && <p className="text-xs text-muted-foreground">{t('approval.filteredInput')}</p>}
        {kind === 'input' && <Label className="flex items-center gap-2 text-sm"><Checkbox checked={readable} onCheckedChange={value => setReadable(value === true)}/>{t('approval.readableInput')}</Label>}
        {!['collecting', 'complete'].includes(record.recording_state) && <p role="status" className="rounded border p-3 text-sm">{t(`approval.recording.${record.recording_state}`, { defaultValue: t('approval.recording.partial') })}</p>}
        {query.isLoading && <p>{t('common.loading')}</p>}
        {query.error && <p role="alert">{t('approval.failed')}</p>}
        {!query.isLoading && !query.error && parts.length === 0 && <p className="text-sm text-muted-foreground">{t(record.recording_state === 'collecting' ? 'approval.contentPending' : kind === 'reasoning' ? 'approval.noReasoning' : 'approval.noContent')}</p>}
        {groupApprovalParts(parts).map(([attempt, entries]) => <section key={attempt} className="space-y-2"><h5 className="text-sm font-medium">{t('approval.attempt', { count: attempt + 1 })}</h5><AttemptParts parts={entries} kind={kind} readable={readable} complete={!query.hasNextPage}/></section>)}
        {query.hasNextPage && <Button type="button" variant="outline" disabled={query.isFetchingNextPage} onClick={() => void query.fetchNextPage()}>{t('approval.loadMore')}</Button>}
    </div>;
}
function ReviewContent(props: { session: string; record: ApprovalReviewDto; identity: string; api: ApprovalApi; onAccessLost: () => void }) {
    const { t } = useTranslation(); const [tab, setTab] = useState('input');
    return <div className="space-y-3">
        <div role="tablist" aria-label={t('approval.details')} className="flex gap-2 overflow-x-auto">
            {['input', 'response', 'usage', 'event'].map(value => <Button type="button" key={value} role="tab" aria-selected={tab === value} variant={tab === value ? 'default' : 'outline'} onClick={() => setTab(value)}>{t(`approval.part.${value}`)}</Button>)}
        </div>
        {tab === 'response' ? <><h4 className="text-sm font-medium">{t('approval.part.reasoning')}</h4><ReviewParts key="reasoning" {...props} kind="reasoning"/><h4 className="text-sm font-medium">{t('approval.part.answer')}</h4><ReviewParts key="answer" {...props} kind="answer"/><h4 className="text-sm font-medium">{t('approval.parsedDecision')}</h4><ReviewParts key="decision" {...props} kind="decision"/></> : <ReviewParts key={tab} {...props} kind={tab}/>}
    </div>;
}
export function ApprovalReviews({ session, open, api = approvalApi, includeChildren = true, onLocate }: { session: string | null; open: boolean; api?: ApprovalApi; includeChildren?: boolean; onLocate?: (record: ApprovalReviewDto) => void }) {
    const { allowed, identity } = useOwnApprovalAccess();
    const { t } = useTranslation();
    const client = useQueryClient();
    const [selected, setSelected] = useState<string | null>(null);
    const [children, setChildren] = useState(includeChildren);
    const [blocked, setBlocked] = useState(false);
    const key = ['approval-own', location.origin, identity, session] as const;
    const list = useInfiniteQuery({ queryKey: [...key, 'list', children], enabled: allowed && !blocked && open && !!session,
        initialPageParam: undefined as string | undefined,
        queryFn: ({ signal, pageParam }) => api.list({ session: session!, include_children: children, cursor: pageParam, limit: 30 }, signal),
        getNextPageParam: last => last.next_cursor ?? undefined, retry: false, gcTime: 0,
    });
    const detail = useQuery({ queryKey: [...key, selected], queryFn: ({ signal }) => api.detail({ session: session!, id: selected! }, signal), enabled: allowed && !blocked && open && !!session && !!selected, retry: false, gcTime: 0, refetchInterval: open ? 3000 : false });
    useEffect(() => { setSelected(null); setBlocked(false); setChildren(includeChildren); return () => { void client.cancelQueries({ queryKey: key }); client.removeQueries({ queryKey: key }); }; }, [client, identity, session]);
    useEffect(() => { if (!open) { void client.cancelQueries({ queryKey: key }); client.removeQueries({ queryKey: key }); } }, [client, open, identity, session]);
    const accessFailed = approvalAccessError(list.error) || approvalAccessError(detail.error);
    useEffect(() => { if (accessFailed || blocked) { setBlocked(true); setSelected(null); void client.cancelQueries({ queryKey: key }); client.removeQueries({ queryKey: key }); } }, [accessFailed, blocked, client, identity, session]);
    if (!allowed || !open) return null;
    if (accessFailed || blocked) return <p role="alert">{t('approval.noAccess')}</p>;
    const record = detail.error ? undefined : detail.data;
    return <section className="space-y-3 rounded border p-3" aria-label={t('approval.records')}>
        <div className="flex items-center justify-between gap-2"><h3 className="flex items-center gap-2 text-sm font-medium"><Bot className="size-4"/>{t('approval.records')}</h3><Button type="button" variant="ghost" size="sm" aria-label={t('approval.refresh')} onClick={() => void client.invalidateQueries({ queryKey: key })}><RefreshCw className="size-4"/></Button></div>
        <p className="text-xs text-muted-foreground">{t('approval.ownOnly')}</p>
        {!session && <p>{t('approval.noSession')}</p>}
        {(list.isLoading || detail.isLoading) && <p>{t('common.loading')}</p>}
        {(list.error || detail.error) && <p role="alert">{t('approval.failed')}</p>}
        {selected ? <>
            <Button type="button" size="sm" variant="ghost" onClick={() => setSelected(null)}><ArrowLeft className="mr-2 size-4"/>{t('approval.back')}</Button>
            {record && <>
                <dl className="grid gap-2 text-xs sm:grid-cols-2">
                    {[[t('approval.outcome'), t(`approval.outcome.${record.status}`, { defaultValue: t('approval.unknown') })], [t('approval.sessionKind'), t(`approval.sessionKind.${record.session_kind}`)], [t('approval.source'), t(`approval.source.${record.source_kind}`, { defaultValue: t('approval.unknown') })], [t('approval.model'), record.model_name ?? '—'], [t('approval.protocol'), record.protocol ?? '—'], [t('approval.revision'), record.model_revision], [t('approval.outputLimit'), record.output_limit ?? '—'], [t('approval.stopReason'), record.stop_reason ?? '—'], [t('approval.started'), time(record.created_at_ms)], [t('approval.finished'), time(record.finished_at_ms)]].map(([label, value]) => <div key={label}><dt className="text-muted-foreground">{label}</dt><dd className="break-all">{value}</dd></div>)}
                </dl>
                <p className="text-xs">{record.session_label ?? record.session}</p>
                {onLocate && <Button type="button" variant="outline" size="sm" onClick={() => onLocate(record)}>{t('approval.locate')}</Button>}
                <p className="text-xs">{t('approval.businessFinished')}: {time(record.business_finished_at_ms)}</p>
                {record.decision && <div><h4 className="text-sm font-medium">{t('approval.storedDecision')}</h4><pre className="whitespace-pre-wrap break-words text-xs">{JSON.stringify(record.decision, null, 2)}</pre></div>}
                <p className="text-xs">{t('approval.grants')}: {record.grant_states.length ? record.grant_states.map(state => t(`approval.grant.${state}`, { defaultValue: t('approval.unknown') })).join(' · ') : t('approval.noGrant')}</p>
                <p className="text-xs">{t('approval.execution')}: {record.execution_state ? t(`approval.execution.${record.execution_state}`, { defaultValue: t('approval.unknown') }) : t('approval.noExecution')}</p>
                {record.timeline.length > 0 && <ul className="space-y-1 text-xs">{record.timeline.map((stage, index) => <li key={index}>{stage.attempt_index != null && <>{t('approval.attempt', { count: stage.attempt_index + 1 })} · </>}{time(stage.at_ms)} · {t(`approval.origin.${stage.origin}`)} · {t(`approval.stage.${stage.stage}`)}</li>)}</ul>}
                {record.reason && <p role="alert" className="text-sm">{record.reason.startsWith('format_retries_exhausted:') ? t('approval.formatExhausted', { count: Number(record.reason.split(':')[1]) }) : t(`approval.reason.${record.reason}`, { defaultValue: t('approval.reason.unknown') })}</p>}
                <p className="text-xs text-muted-foreground">{t('approval.approvedNotExecuted')}</p>
                {record.usage && <pre className="whitespace-pre-wrap break-words rounded bg-muted/30 p-2 text-xs">{JSON.stringify(record.usage, null, 2)}</pre>}
                <ReviewContent onAccessLost={() => setBlocked(true)} key={record.id} session={session!} record={record} identity={identity} api={api}/>
            </>}
        </> : <>
            {includeChildren && <Label className="flex items-center gap-2 text-xs"><Checkbox checked={children} onCheckedChange={value => setChildren(value === true)}/>{t('approval.includeChildren')}</Label>}
            {(!list.error ? list.data : undefined)?.pages.flatMap(page => page.reviews).map(record => <Button key={record.id} type="button" variant="outline" className="h-auto w-full justify-start gap-2 whitespace-normal py-3 text-left" onClick={() => setSelected(record.id)}><FileSearch className="size-4 shrink-0"/><span>{t(`approval.sessionKind.${record.session_kind}`)} · {record.session_label ?? record.session} · {t(`approval.source.${record.source_kind}`, { defaultValue: t('approval.unknown') })} · {t(`approval.outcome.${record.status}`, { defaultValue: t('approval.unknown') })}<br/><span className="text-xs text-muted-foreground">{time(record.created_at_ms)}</span></span></Button>)}
            {list.data?.pages.every(page => page.reviews.length === 0) && <p>{t('approval.noRecords')}</p>}
            {list.hasNextPage && <Button type="button" variant="outline" onClick={() => void list.fetchNextPage()} disabled={list.isFetchingNextPage}>{t('approval.loadMore')}</Button>}
        </>}
    </section>;
}
