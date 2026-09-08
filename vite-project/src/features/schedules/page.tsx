import { ResumeSourcePicker } from './resume-source-picker';
import { ProposalReview } from './proposal-review';
import { useCallback, useEffect, useMemo, useRef, useState, type FormEvent } from 'react';
import { useSearchParams } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { v4 } from 'uuid';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { useDeskSignaling } from '@/features/desk/use-desk-signaling';
import type { ScheduleManagementRequest, ScheduleSpec, ScheduleView, ScheduleTimeConversion } from '@/services/types';
import { ScheduleClient, ScheduleRequestError } from './client';
import { formatTime, ruleTimes, validTimezone, projectRule } from './time';
import { RunHistory } from './run-history';
import { RehearsalDetails } from './rehearsal-details';
import { ContractReview } from './contract-review';

export type ScheduleDevice = { id: string; name: string; connectionId?: string; assistantPath?: string };
type ResumeSource = { conversation: string; device: string; revision: number };
type Editor = { resume?: ResumeSource; kind: 'create' | 'time' | 'rename' | 'delete' | 'revoke' | 'editPrompt' | 'failureThreshold'; task?: ScheduleView; key: string };
const selectClass = 'h-10 w-full rounded-md border bg-background px-3 text-sm';

export default function SchedulePage({ devices, loadingDevices = false }: { devices: ScheduleDevice[]; loadingDevices?: boolean }) {
    const { t, i18n } = useTranslation();
    const { isConnected, subscribe, sendTracked, cancelQueued } = useDeskSignaling();
    const client = useMemo(() => new ScheduleClient(sendTracked, cancelQueued), [sendTracked, cancelQueued]);
    const [tasks, setTasks] = useState<ScheduleView[]>([]);
    const [counts, setCounts] = useState<{ total: number; attention: number } | null>(null);
    const [cursor, setCursor] = useState<string | null>(null);
    const [choosingConversation, setChoosingConversation] = useState(false);
    const [tab, setTab] = useState<'fresh_task' | 'conversation_resume'>('fresh_task');
    const [filterTitle, setFilterTitle] = useState('');
    const [filterDevice, setFilterDevice] = useState('');
    const [filterStatus, setFilterStatus] = useState<ScheduleView['status'] | ''>('');
    const [attentionOnly, setAttentionOnly] = useState(false);
    const [zone, setZone] = useState(() => Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC');
    const [error, setError] = useState('');
    const [busy, setBusy] = useState(false);
    const [editor, setEditor] = useState<Editor | null>(null);
    const [search, setSearch] = useSearchParams();
    const sourceConversation = search.get('resume_conversation');
    const sourceDevice = search.get('resume_device');
    const sourceRevision = Number(search.get('resume_revision'));
    const resumeSource = sourceConversation && sourceConversation.length <= 256 && sourceDevice && sourceDevice.length <= 256 && Number.isSafeInteger(sourceRevision) && sourceRevision > 0
        ? { conversation: sourceConversation, device: sourceDevice, revision: sourceRevision } : null;
    const reviewSelector = search.get('review_task');
    const validReviewSelector = reviewSelector && /^[a-f0-9-]{36}$/.test(reviewSelector) ? reviewSelector : null;
    const taskSelector = search.get('scheduled_task');
    const runSelector = search.get('scheduled_run');
    const invalidHistoryLink = (taskSelector !== null && (!taskSelector || taskSelector.length > 256))
        || (runSelector !== null && (!runSelector || runSelector.length > 256 || !taskSelector));
    const historyTask = invalidHistoryLink ? null : taskSelector;
    const openHistory = (taskId: string | null, runId: string | null = null) => {
        const next = new URLSearchParams(search);
        next.delete('scheduled_task'); next.delete('scheduled_run');
        if (taskId) next.set('scheduled_task', taskId);
        if (taskId && runId) next.set('scheduled_run', runId);
        setSearch(next);
    };
    const [rehearsalTask, setRehearsalTask] = useState<ScheduleView | null>(null);
    const [contractTask, setContractTask] = useState<string | null>(null);
    const generation = useRef(0);
    const mounted = useRef(true);
    const report = useCallback((err: unknown) => {
        if (mounted.current) setError(err instanceof ScheduleRequestError && err.reason === 'server' ? err.message : t('schedules.requestFailed'));
    }, [t]);
    useEffect(() => {
        mounted.current = true;
        const unsubscribe = subscribe(client.receive);
        return () => { mounted.current = false; unsubscribe(); client.close(); };
    }, [client, subscribe]);
    const load = useCallback(async (after: string | null = null) => {
        const version = ++generation.current;
        setBusy(true); setError('');
        try {
            const response = await client.request({ operation: 'search', after, limit: 50, kind: tab,
                title: filterTitle || null, status: filterStatus || null, target_device_id: filterDevice || null, attention_only: attentionOnly });
            if (!mounted.current || version !== generation.current || response.result !== 'search_results') return;
            setTasks(previous => after ? [...previous.filter(task => !response.tasks.some(next => next.schedule_id === task.schedule_id)), ...response.tasks] : response.tasks);
            setCursor(response.next_cursor ?? null);
            setCounts({ total: response.total, attention: response.attention_count });
        } catch (err) { if (version === generation.current) report(err); }
        finally { if (mounted.current && version === generation.current) setBusy(false); }
    }, [client, report, tab, filterTitle, filterStatus, filterDevice, attentionOnly]);
    useEffect(() => {
        if (isConnected) { setTasks([]); setCursor(null); setCounts(null); void load(); }
        else { client.close(); ++generation.current; setBusy(false); setCounts(null); }
    }, [isConnected, client, load]);
    const mutate = async (request: ScheduleManagementRequest) => {
        ++generation.current; setBusy(true); setError('');
        try {
            const response = await client.request(request);
            if (!mounted.current) return;
            if (response.result !== 'task') throw new ScheduleRequestError('invalid');
            const task = response.task;
            setTasks(previous => task.status === 'deleted' ? previous.filter(item => item.schedule_id !== task.schedule_id) : [task, ...previous.filter(item => item.schedule_id !== task.schedule_id)]);
            setEditor(null);
            await load();
        } catch (err) { report(err); throw err; }
        finally { if (mounted.current) setBusy(false); }
    };
    const validZone = validTimezone(zone);
    const available = isConnected && !busy;
    const visible = tasks.filter(task => task.kind === tab);
    const allDevices = [...devices];
    for (const task of tasks) if (task.target_device_id && !allDevices.some(d => d.id === task.target_device_id)) allDevices.push({ id: task.target_device_id, name: task.target_device_id });
    return <section className="space-y-5">
        <div className="flex flex-wrap items-center justify-between gap-3">
            <h1 className="text-2xl font-semibold">{t('schedules.title')}</h1>
            <div className="flex gap-2">
                <Button variant="outline" disabled={!available} onClick={() => void load()}>{t('schedules.refresh')}</Button>
                <Button disabled={!available || loadingDevices || !allDevices.length} onClick={() => setChoosingConversation(true)}>{t('schedules.chooseConversation')}</Button>
                {resumeSource && <Button disabled={!available || !devices.some(device => device.id === resumeSource.device)} onClick={() => { setTab('conversation_resume'); setEditor({ kind: 'create', resume: resumeSource, key: v4() }); }}>{t('schedules.createResume')}</Button>}
                <Button disabled={!available || loadingDevices || !allDevices.length} onClick={() => { setTab('fresh_task'); setEditor({ kind: 'create', key: v4() }); }}>{t('schedules.create')}</Button>
            </div>
        </div>
        <p className="text-sm text-muted-foreground">{t('schedules.draftNote')}</p>
        <div className="max-w-sm space-y-1"><Label htmlFor="schedule-zone">{t('schedules.timezone')}</Label><Input id="schedule-zone" value={zone} onChange={e => setZone(e.target.value)} /></div>
        {!validZone && <p role="alert">{t('schedules.invalidZone')}</p>}
        <p className="text-sm text-muted-foreground">{t('schedules.utcNote')}</p>
        {!isConnected && <p role="status">{t('schedules.connecting')}</p>}
        {invalidHistoryLink && <p role="alert">{t('schedules.result.invalidLink')}</p>}
        {error && <p role="alert" className="text-destructive">{error}</p>}
        <div className="flex gap-2" role="tablist" aria-label={t('schedules.title')}>
            {(['fresh_task', 'conversation_resume'] as const).map(kind => <Button key={kind} role="tab" aria-selected={tab === kind} variant={tab === kind ? 'default' : 'outline'} onClick={() => setTab(kind)}>{t(`schedules.${kind}`)}</Button>)}
        </div>
        <div className="grid gap-3 sm:grid-cols-3">
            <label className="space-y-1">{t('schedules.filters.name')}
                <Input value={filterTitle} maxLength={256} onChange={event => setFilterTitle(event.target.value)} />
            </label>
            <label className="space-y-1">{t('schedules.filters.device')}
                <select className={selectClass} value={filterDevice} onChange={event => setFilterDevice(event.target.value)}>
                    <option value="">{t('schedules.filters.allDevices')}</option>
                    {devices.map(device => <option key={device.id} value={device.id}>{device.name}</option>)}
                </select>
            </label>
            <label className="space-y-1">{t('schedules.filters.status')}
                <select className={selectClass} value={filterStatus} onChange={event => setFilterStatus(event.target.value as ScheduleView['status'] | '')}>
                    <option value="">{t('schedules.filters.allStatuses')}</option>
                    {(['draft', 'rehearsing', 'awaiting_authorization', 'active', 'paused', 'triggered', 'completed'] as const).map(status =>
                        <option key={status} value={status}>{t(`schedules.status.${status}`)}</option>)}
                </select>
            </label>
            <label className="flex items-center gap-2"><input type="checkbox" checked={attentionOnly} onChange={event => setAttentionOnly(event.target.checked)} />{t('schedules.filters.attention')}{counts && ` (${counts.attention})`}</label>
        </div>
        {counts && <p className="text-sm text-muted-foreground">{t('schedules.filters.total', { count: counts.total })}</p>}
        <div role="tabpanel" className="space-y-3" aria-busy={busy}>
            {!visible.length && <p className="py-8 text-muted-foreground">{t(busy ? 'schedules.loading' : 'schedules.empty')}</p>}
            {visible.map(task => <article key={task.schedule_id} className="space-y-3 rounded-xl border p-4">
                <div className="flex flex-wrap justify-between gap-2"><h2 className="font-semibold">{task.title}</h2><span className="text-sm">{t(`schedules.status.${task.status}`)}</span></div>
                <p className="whitespace-pre-wrap text-sm">{task.prompt}</p>
                <p className="text-sm text-muted-foreground">{allDevices.find(d => d.id === task.target_device_id)?.name ?? t('schedules.targetUnavailable')}</p>
                {validZone && <p className="text-sm">{task.spec.rule.kind === 'interval' ? t('schedules.everySeconds', { count: task.spec.rule.every_seconds }) : `${t(`schedules.rule.${task.spec.rule.kind}`)} · ${ruleTimes(task.spec, zone, i18n.language, task.next_run_at ? new Date(task.next_run_at) : undefined).join(' / ')}`}</p>}
                <p className="text-sm">{t('schedules.next')}: {validZone && task.next_run_at ? formatTime(task.next_run_at, zone, i18n.language) : t('schedules.notScheduled')}</p>
                {validZone && task.upcoming_runs.length > 0 && <details className="text-sm"><summary>{t('schedules.timePreview.upcoming')}</summary>
                    <p>{t('schedules.timePreview.projection')}</p>
                    {task.upcoming_runs.map(at => <p key={at}>{formatTime(at, zone, i18n.language)}</p>)}
                </details>}
                <p className="text-sm">{t('schedules.failureCount', { failures: task.consecutive_failures, threshold: task.failure_threshold })}</p>
                {task.pause_reasons.map(reason => <p key={reason} className="text-sm">{t(`schedules.pause.${reason}`)}</p>)}
                <div className="flex flex-wrap gap-2">
                    <Button variant="outline" disabled={!available} onClick={() => openHistory(task.schedule_id)}>{t('schedules.history.title')}</Button>
                    {task.kind === 'conversation_resume' && task.status === 'draft' && <Button disabled={!available} onClick={() => void mutate({ operation: 'activate_conversation_resume', schedule_id: task.schedule_id, expected_revision: task.revision }).catch(() => {})}>{t('schedules.activateResume')}</Button>}
                    {task.kind === 'fresh_task' && <Button variant="outline" disabled={!available} onClick={() => setContractTask(task.schedule_id)}>{t('schedules.contract.title')}</Button>}
                    {task.kind === 'fresh_task' && ['active', 'triggered', 'paused', 'completed'].includes(task.status)
                        && !task.pause_reasons.includes('authorization_invalid') && <Button variant="outline" disabled={!available}
                            onClick={() => setEditor({ kind: 'revoke', task, key: v4() })}>{t('schedules.revoke')}</Button>}
                    {task.kind === 'fresh_task' && <Button variant="outline" disabled={!available} onClick={() => setRehearsalTask(task)}>{t('schedules.rehearsal.view')}</Button>}
                    {!['completed', 'deleted'].includes(task.status) && <Button variant="outline" disabled={!available || !!task.active_run_id}
                        onClick={() => setEditor({ kind: 'failureThreshold', task, key: v4() })}>{t('schedules.failureThreshold')}</Button>}
                    {!['completed', 'deleted'].includes(task.status) && <Button variant="outline" disabled={!available || (task.kind === 'conversation_resume' && !!task.active_run_id)}
                        onClick={() => setEditor({ kind: 'editPrompt', task, key: v4() })}>{t('schedules.editPrompt')}</Button>}
                    {(['rename', 'time', 'delete'] as const).filter(kind => kind !== 'time' || !['completed', 'deleted'].includes(task.status)).map(kind => <Button key={kind} variant="outline" disabled={!available} onClick={() => setEditor({ kind, task, key: v4() })}>{t(`schedules.${kind}`)}</Button>)}
                    {task.kind === 'fresh_task' && task.status === 'active' && <Button variant="outline"
                        disabled={!available || !!task.active_run_id || task.pause_reasons.length > 0}
                        onClick={() => void mutate({ operation: 'run_task_now', schedule_id: task.schedule_id, expected_revision: task.revision, client_request_key: v4() }).catch(() => {})}>{t('schedules.runNow')}</Button>}
                    {task.status === 'paused'  && <Button variant="outline"
                        disabled={!available || !!task.active_run_id || task.pause_reasons.includes('unknown_side_effect') || task.pause_reasons.includes('schedule_upgrade_required')}
                        onClick={() => void mutate({ operation: 'resume_task', schedule_id: task.schedule_id, expected_revision: task.revision }).catch(() => {})}>{t('schedules.resumeTask')}</Button>}
                    {['active', 'triggered'].includes(task.status) && <Button variant="outline" disabled={!available} onClick={() => void mutate({ operation: 'pause', schedule_id: task.schedule_id, expected_revision: task.revision }).catch(() => {})}>{t('schedules.pauseTask')}</Button>}
                </div>
            </article>)}
        </div>
        <Dialog open={!!validReviewSelector} onOpenChange={open => { if (!open) { const next = new URLSearchParams(search); next.delete('review_task'); setSearch(next); void load(); } }}><DialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto" aria-describedby={undefined}>
            <DialogHeader><DialogTitle>{t('schedules.proposal.open')}</DialogTitle></DialogHeader>
            {validReviewSelector && <ProposalReview key={validReviewSelector} client={client} scheduleId={validReviewSelector} connected={isConnected} zone={zone}
                assistantPaths={Object.fromEntries(devices.filter(device => device.assistantPath).map(device => [device.id, device.assistantPath!]))} onChanged={() => void load()} />}
        </DialogContent></Dialog>
        <Dialog open={!!contractTask} onOpenChange={open => { if (!open) setContractTask(null); }}><DialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto" aria-describedby={undefined}>
            <DialogHeader><DialogTitle>{t('schedules.contract.title')}</DialogTitle></DialogHeader>
            {contractTask && <ContractReview client={client} scheduleId={contractTask} connected={isConnected} onPublished={task => { ++generation.current; setTasks(previous => previous.map(item => item.schedule_id === task.schedule_id ? task : item)); }} />}
        </DialogContent></Dialog>
        {cursor && <Button variant="outline" disabled={!available} onClick={() => void load(cursor)}>{t('schedules.more')}</Button>}
        <Dialog open={!!historyTask} onOpenChange={open => { if (!open) openHistory(null); }}><DialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto" aria-describedby={undefined}>
            <DialogHeader><DialogTitle>{t('schedules.history.title')}</DialogTitle></DialogHeader>
            {historyTask && <RunHistory key={historyTask} client={client} scheduleId={historyTask} connectionIds={Object.fromEntries(devices.filter(d => d.connectionId).map(d => [d.id, d.connectionId!]))} connected={isConnected} zone={zone} selectedRun={runSelector} onSelectRun={run => openHistory(historyTask, run)} />}
        </DialogContent></Dialog>
        <Dialog open={!!rehearsalTask} onOpenChange={open => { if (!open) setRehearsalTask(null); }}><DialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto" aria-describedby={undefined}>
            <DialogHeader><DialogTitle>{t('schedules.rehearsal.view')}</DialogTitle></DialogHeader>
            {rehearsalTask && <RehearsalDetails key={rehearsalTask.schedule_id} client={client} scheduleId={rehearsalTask.schedule_id} connected={isConnected} zone={zone} assistantPaths={Object.fromEntries(allDevices.filter(device => device.assistantPath).map(device => [device.id, device.assistantPath!]))} />}
        </DialogContent></Dialog>
        <Dialog open={choosingConversation} onOpenChange={setChoosingConversation}><DialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto" aria-describedby={undefined}>
            <DialogHeader><DialogTitle>{t('schedules.chooseConversation')}</DialogTitle></DialogHeader>
            {choosingConversation && <ResumeSourcePicker client={client} devices={allDevices} connected={available} onSelect={source => {
                setChoosingConversation(false); setTab('conversation_resume');
                setEditor({ kind: 'create', key: v4(), resume: { conversation: source.client_conversation_id, device: source.target_device_id, revision: source.requirement_revision } });
            }} />}
        </DialogContent></Dialog>
        <Dialog open={!!editor} onOpenChange={open => { if (!open && !busy) setEditor(null); }}><DialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto" aria-describedby={undefined}>
            <DialogHeader><DialogTitle>{editor && t(editor.resume ? 'schedules.createResume' : `schedules.${editor.kind}`)}</DialogTitle></DialogHeader>
            {editor && <ScheduleEditor key={editor.key} editor={editor} devices={allDevices} zone={zone} disabled={!available || !validZone} client={client} submit={mutate} report={report} close={() => setEditor(null)} />}
        </DialogContent></Dialog>
    </section>;
}

function ScheduleEditor({ editor, devices, zone, disabled, client, submit, report, close }: { close: () => void; editor: Editor; devices: ScheduleDevice[]; zone: string; disabled: boolean; client: ScheduleClient; submit: (request: ScheduleManagementRequest) => Promise<void>; report: (error: unknown) => void }) {
    const { t, i18n } = useTranslation();
    const [title, setTitle] = useState(editor.task?.title ?? '');
    const [prompt, setPrompt] = useState(editor.task?.prompt ?? '');
    const [threshold, setThreshold] = useState(String(editor.task?.failure_threshold ?? 3));
    const [device, setDevice] = useState(editor.resume?.device ?? devices[0]?.id ?? '');
    const reference = useRef(new Date());
    const projection = useMemo(() => editor.kind === 'time' && editor.task && validTimezone(zone)
        ? projectRule(editor.task.spec, zone, reference.current) : null, [editor.kind, editor.task, zone]);
    const [kind, setKind] = useState<'once' | 'daily' | 'weekly' | 'interval'>(projection?.kind ?? (editor.resume || editor.task?.kind === 'conversation_resume' ? 'once' : 'daily'));
    const [date, setDate] = useState(projection?.date ?? '');
    const [time, setTime] = useState(projection?.time ?? '');
    const [days, setDays] = useState<number[]>(projection?.days ?? [1]);
    const [seconds, setSeconds] = useState(projection?.seconds ?? 86400);
    const [fold, setFold] = useState<'earlier' | 'later' | ''>('');
    const [saving, setSaving] = useState(false);
    const [error, setError] = useState('');
    const frozen = useRef<ScheduleManagementRequest | null>(null);
    const alive = useRef(true);
    useEffect(() => { alive.current = true; return () => { alive.current = false; }; }, []);
    const timeForm = editor.kind === 'create' || editor.kind === 'time';
    const timeKey = JSON.stringify([zone, date, time, kind, [...days].sort(), seconds, fold]);
    const latestTimeKey = useRef(timeKey);
    latestTimeKey.current = timeKey;
    const [timePreview, setTimePreview] = useState<{ key: string; spec: ScheduleSpec; version: string; offset: number; upcoming: string[] } | null>(null);
    const previewMatches = timePreview?.key === timeKey;
    useEffect(() => {
        if (!projection) return;
        setKind(projection.kind); setDate(projection.date); setTime(projection.time);
        setDays(projection.days); setSeconds(projection.seconds); setFold(''); setTimePreview(null);
    }, [projection]);
    const unchangedTime = projection !== null && kind === projection.kind && date === projection.date
        && (time.length === 5 ? time + ':00' : time) === projection.time
        && JSON.stringify([...days].sort()) === JSON.stringify(projection.days) && seconds === projection.seconds && !fold;

    const onSubmit = async (event: FormEvent) => {
        event.preventDefault();
        if (disabled || saving) return;
        if (editor.kind === 'time' && unchangedTime && !frozen.current) { close(); return; }
        setError('');
        setSaving(true);
        try {
            if (frozen.current) { await submit(frozen.current); return; }
            const task = editor.task;
            let request: ScheduleManagementRequest;
            if (editor.kind === 'delete' && task) request = { operation: 'delete', schedule_id: task.schedule_id, expected_revision: task.revision };
            else if (editor.kind === 'revoke' && task) request = { operation: 'revoke_task_authorization', schedule_id: task.schedule_id, expected_revision: task.revision };
            else if (editor.kind === 'failureThreshold' && task) {
                const value = Number(threshold);
                if (!Number.isInteger(value) || value < 1 || value > 4294967295) throw new ScheduleRequestError('invalid');
                request = { operation: 'set_failure_threshold', schedule_id: task.schedule_id, expected_revision: task.revision, failure_threshold: value };
            }
            else if (editor.kind === 'editPrompt' && task) request = { operation: 'change_prompt', schedule_id: task.schedule_id, expected_revision: task.revision, prompt };
            else if (editor.kind === 'rename' && task) request = { operation: 'rename', schedule_id: task.schedule_id, expected_revision: task.revision, title };
            else {
                const input: ScheduleTimeConversion = { timezone: zone, reference_date: date, local_time: time.length === 5 ? time + ':00' : time, fold: fold || null,
                    rule: kind === 'weekly' ? { kind, weekdays: days } : kind === 'interval' ? { kind, every_seconds: seconds } : { kind } };
                const converted = await client.request({ operation: 'convert_time', input });
                if (!alive.current) return;
                if (converted.result !== 'converted_time') throw new ScheduleRequestError('invalid');
                const spec = converted.conversion.spec;
                if (spec.rule.kind !== kind || !Array.isArray(converted.upcoming_runs) || converted.upcoming_runs.length > 5) throw new ScheduleRequestError('invalid');
                if (latestTimeKey.current !== timeKey) return;
                const nextPreview = { key: timeKey, spec, version: converted.conversion.conversion_version, offset: converted.conversion.offset_seconds, upcoming: converted.upcoming_runs };
                if (!nextPreview.version || !Number.isInteger(nextPreview.offset)) throw new ScheduleRequestError('invalid');
                if (!previewMatches || !timePreview || timePreview.version !== nextPreview.version || timePreview.offset !== nextPreview.offset || JSON.stringify(timePreview.spec) !== JSON.stringify(spec)) {
                    setTimePreview(nextPreview);
                    if (previewMatches) setError(t('schedules.timePreview.changed'));
                    return;
                }
                const time_confirmation = { input, conversion_version: converted.conversion.conversion_version };
                request = task ? { operation: 'change_time', schedule_id: task.schedule_id, expected_revision: task.revision, spec, time_confirmation } : { operation: 'create_draft', draft: { client_create_key: editor.key, kind: editor.resume ? 'conversation_resume' : 'fresh_task', target_device_id: device, title, prompt, spec, time_confirmation, creation_source: 'manual', locale: null, model_id: null, source_conversation_id: editor.resume?.conversation ?? null, requirement_revision: editor.resume?.revision ?? null } };
            }
            frozen.current = request;
            await submit(request);
        } catch (err) {
            if (alive.current) {
                setError(err instanceof ScheduleRequestError && err.reason === 'server' ? err.message : t('schedules.requestFailed'));
                report(err);
            }
        }
        finally { if (alive.current) setSaving(false); }
    };
    const locked = disabled || saving || !!frozen.current;
    return <form onSubmit={event => void onSubmit(event)} className="space-y-3">
        {editor.kind === 'revoke' ? <p>{t('schedules.revokeConfirm', { title: editor.task?.title })}</p> : editor.kind === 'delete' ? <p>{t('schedules.deleteConfirm', { title: editor.task?.title })}</p> : <fieldset disabled={locked} className="space-y-3">
            {['create', 'rename'].includes(editor.kind) && <div><Label htmlFor="schedule-title">{t('schedules.name')}</Label><Input id="schedule-title" required maxLength={240} value={title} onChange={e => setTitle(e.target.value)} /></div>}
            {editor.kind === 'create' && <>
                <div><Label htmlFor="schedule-device">{t('schedules.device')}</Label><select id="schedule-device" disabled={!!editor.resume} className={selectClass} value={device} onChange={e => setDevice(e.target.value)}>{devices.map(d => <option key={d.id} value={d.id}>{d.name}</option>)}</select></div>
                <div><Label htmlFor="schedule-prompt">{t('schedules.prompt')}</Label><textarea id="schedule-prompt" required maxLength={32768} className="min-h-24 w-full rounded-md border bg-background p-2" value={prompt} onChange={e => setPrompt(e.target.value)} /></div>
            </>}
            {editor.kind === 'failureThreshold' && <>
                <p>{t('schedules.failureThresholdNote')}</p>
                <Label htmlFor="schedule-threshold">{t('schedules.failureThreshold')}</Label>
                <Input id="schedule-threshold" type="number" min={1} max={4294967295} step={1} required value={threshold} onChange={event => setThreshold(event.target.value)} />
            </>}
            {editor.kind === 'editPrompt' && <>
                <p>{t(editor.task?.kind === 'conversation_resume' ? 'schedules.editPromptResumeNote' : 'schedules.editPromptFreshNote')}</p>
                <Label htmlFor="schedule-prompt">{t('schedules.prompt')}</Label>
                <textarea id="schedule-prompt" required maxLength={16384} className="min-h-32 w-full rounded-md border bg-background p-2" value={prompt} onChange={e => setPrompt(e.target.value)} />
            </>}
            {timeForm && <>
                <div><Label htmlFor="schedule-rule">{t('schedules.frequency')}</Label><select id="schedule-rule" className={selectClass} value={kind} onChange={e => setKind(e.target.value as typeof kind)}>{(editor.resume || editor.task?.kind === 'conversation_resume' ? ['once'] as const : ['once', 'daily', 'weekly', 'interval'] as const).map(value => <option key={value} value={value}>{t(`schedules.rule.${value}`)}</option>)}</select></div>
                <p className="text-sm text-muted-foreground">{t('schedules.dateNote', { zone })}</p>
                <div className="grid grid-cols-2 gap-3"><div><Label htmlFor="schedule-date">{t('schedules.date')}</Label><Input id="schedule-date" type="date" required value={date} onChange={e => setDate(e.target.value)} /></div><div><Label htmlFor="schedule-time">{t('schedules.clock')}</Label><Input id="schedule-time" type="time" step="1" required value={time} onChange={e => setTime(e.target.value)} /></div></div>
                {kind === 'weekly' && <div className="flex flex-wrap gap-3">{[1, 2, 3, 4, 5, 6, 7].map(day => <label key={day} className="flex items-center gap-1"><input type="checkbox" checked={days.includes(day)} onChange={e => setDays(old => e.target.checked ? [...old, day] : old.filter(d => d !== day))} />{t(`schedules.day.${day}`)}</label>)}</div>}
                {kind === 'interval' && <div><Label htmlFor="schedule-seconds">{t('schedules.seconds')}</Label><Input id="schedule-seconds" type="number" min="60" required value={seconds} onChange={e => setSeconds(Number(e.target.value))} /></div>}
                <div><Label htmlFor="schedule-fold">{t('schedules.fold')}</Label><select id="schedule-fold" className={selectClass} value={fold} onChange={e => setFold(e.target.value as typeof fold)}><option value="">{t('schedules.foldAsk')}</option><option value="earlier">{t('schedules.foldEarlier')}</option><option value="later">{t('schedules.foldLater')}</option></select></div>
            </>}
        </fieldset>}
        {timeForm && previewMatches && timePreview && <section className="rounded-md border p-3 space-y-2">
            <p>{t('schedules.timePreview.note')}</p>
            {timePreview.spec.rule.kind === 'interval' ? <p>{formatTime(timePreview.spec.rule.anchor_at, 'UTC', i18n.language)} · {t('schedules.seconds')}: {timePreview.spec.rule.every_seconds}</p> :
                ruleTimes(timePreview.spec, 'UTC', i18n.language).map(value => <p key={value}>UTC · {value}</p>)}
            <p>{t('schedules.timePreview.upcoming')}</p>
            {timePreview.upcoming.map(at => <p key={at}>{formatTime(at, zone, i18n.language)}</p>)}
            <p>{t('schedules.utcNote')}</p>
        </section>}
        {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
        {frozen.current && !saving && <p className="text-sm">{t('schedules.retryNote')}</p>}
        <Button type="submit" disabled={disabled || saving}>{t(saving ? 'schedules.saving' : editor.kind === 'delete' ? 'schedules.delete' : editor.kind === 'revoke' ? 'schedules.revoke' : timeForm && !frozen.current ? (unchangedTime ? 'schedules.save' : previewMatches ? 'schedules.timePreview.confirm' : 'schedules.timePreview.show') : 'schedules.save')}</Button>
    </form>;
}
