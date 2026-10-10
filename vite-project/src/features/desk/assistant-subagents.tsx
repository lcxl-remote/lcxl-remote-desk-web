import { useFollowLatest } from '@/hooks/use-follow-latest';
import { AssistantTranscript } from './assistant-transcript';
import { AssistantContextMeter } from './assistant-context-meter';
import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { ClipboardList, ArrowDown, Paperclip, FolderKey, ListTodo, ShieldCheck, ChevronLeft, ChevronRight, ListFilter, Plus } from 'lucide-react';
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger } from '@/components/ui/dropdown-menu';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Disclosure } from '@/components/ui/disclosure';
import { Textarea } from '@/components/ui/textarea';
import { MarkdownContent } from '@/components/markdown-content';
import { formatLocalTime } from '@/lib/local-time';
import { AssistantPermissionRequest } from './assistant-permission-request';
import { AssistantDirectoryApproval } from './assistant-directory-approval';
import { AssistantAttachments } from './assistant-attachments';
import { AssistantPermissionRecords, permissionIsAutomaticallyReviewed } from './assistant-permission-records';
import { AssistantFileScope } from './assistant-file-scope';
import { AssistantBackgroundTasks } from './assistant-background-tasks';
import { AssistantDetailsSheet } from './assistant-details-sheet';
import { AssistantComposerTools } from './assistant-composer-tools';
import { AssistantMoreMenu } from './assistant-more-menu';
import { permissionToolLabel, permissionOperationLabel } from './assistant-permission-labels';
import { subagentIsTerminal, useAiAssistantSubagents } from './use-ai-assistant-subagents';
import type { InteractiveCommandContext } from './assistant-interactive-command';

export function AssistantSubagentApprovalNotice({ agents }: {
    agents: Pick<ReturnType<typeof useAiAssistantSubagents>, 'tasks' | 'openDetail'>;
}) {
    const { t } = useTranslation();
    const waiting = agents.tasks.filter(task => task.state === 'waiting_approval');
    if (waiting.length === 0) return null;
    return <div role="status" className="flex min-w-0 shrink-0 flex-wrap items-center gap-2 rounded-md border border-amber-500/50 bg-amber-500/5 p-2 text-sm">
        <span className="flex items-center gap-2"><ClipboardList className="h-4 w-4 shrink-0" aria-hidden="true" />
            {t('pages.aiAssistant.subagents.approvalCount', { count: waiting.length })}</span>
        {waiting.map(task => <Button key={task.task_id} type="button" size="sm" variant="outline"
            className="h-auto max-w-full whitespace-normal break-words text-left"
            onClick={() => agents.openDetail(task)}>
            {t('pages.aiAssistant.subagents.viewApproval', { name: task.name })}
        </Button>)}
    </div>;
}

export type AssistantSubagentPanel = 'details' | 'capabilities' | 'context' | 'attachments' | 'directories' | 'permissions' | 'tasks';

export function AssistantSubagentMenu({ onSelect, onDeviceSettings, onApprovalSettings }: {
    onSelect: (panel: AssistantSubagentPanel) => void; onDeviceSettings?: () => void; onApprovalSettings?: () => void;
}) {
    const { t } = useTranslation();
    return <AssistantMoreMenu sections={[
        { label: t('pages.aiAssistant.workspace.resources'), actions: [
            { label: t('pages.aiAssistant.attachments.title'), icon: Paperclip, onSelect: () => onSelect('attachments') },
            { label: t('pages.aiAssistant.contextTitle'), onSelect: () => onSelect('context') },
            { label: t('pages.aiAssistant.directories.title'), icon: FolderKey, onSelect: () => onSelect('directories') },
        ] },
        { label: t('pages.aiAssistant.workspace.manage'), actions: [
            { label: t('pages.aiAssistant.tasks.title'), icon: ListTodo, onSelect: () => onSelect('tasks') },
            { label: t('pages.aiAssistant.permissionHistory'), icon: ShieldCheck, onSelect: () => onSelect('permissions') },
            ...(onApprovalSettings ? [{ label: t('pages.aiAssistant.autoApprovalTitle'), icon: ShieldCheck, onSelect: onApprovalSettings }] : []),
            ...(onDeviceSettings ? [{ label: t('pages.aiAssistant.workspace.deviceSettings'), onSelect: onDeviceSettings }] : []),
        ] },
        { label: t('pages.aiAssistant.workspace.troubleshoot'), actions: [
            { label: t('pages.aiAssistant.workspace.details'), onSelect: () => onSelect('details') },
            { label: t('pages.aiAssistant.workspace.capabilities'), onSelect: () => onSelect('capabilities') },
        ] },
    ]} />;
}

export function AssistantSubagents({ agents, connected, canDecide, interactive, exportBackup, panel: externalPanel,
    onPanelChange, deskId, sessionTargetId, capabilityList, adjustment: externalAdjustment, onAdjustmentChange }: {
    agents: ReturnType<typeof useAiAssistantSubagents>; connected: boolean; canDecide: boolean;
    interactive?: Omit<InteractiveCommandContext, 'runId'>;
    mainStopped: boolean; exportBackup?: (session: string, id: string) => Promise<void>;
    panel?: AssistantSubagentPanel | null; onPanelChange?: (panel: AssistantSubagentPanel | null) => void;
    deskId?: string; sessionTargetId?: string | null;
    capabilityList?: ReactNode;
    adjustment?: string; onAdjustmentChange?: (text: string) => void;
}) {
    const { t } = useTranslation();
    const [localAdjustment, setLocalAdjustment] = useState('');
    const adjustment = externalAdjustment ?? localAdjustment;
    const validAdjustment = adjustment.trim().length > 0 && new TextEncoder().encode(adjustment.trim()).length <= 16_384;
    const setAdjustment = onAdjustmentChange ?? setLocalAdjustment;
    const [localPanel, setLocalPanel] = useState<AssistantSubagentPanel | null>(null);
    const panel = externalPanel === undefined ? localPanel : externalPanel;
    const setPanel = onPanelChange ?? setLocalPanel;
    const task = agents.detail?.result.task ?? agents.selected;
    useEffect(() => { setLocalAdjustment(''); setLocalPanel(null); }, [task?.task_id]);
    const child = agents.detail?.session;
    const report = agents.detail?.result.report;
    const { scrollRef, contentRef, onScroll, showJumpToLatest, jumpToLatest } = useFollowLatest(true, child?.sessionId ?? task?.child_session_id ?? '');
    if (!task) return null;
    const pendingDirectories = child?.fileScope?.directories.filter(directory => directory.state === 'pending') ?? [];
    const automaticApproval = child?.approvalDelegation?.status === 'active';
    const pendingPermissions = child?.permissionRequests?.filter(request => ['pending', 'needs_revalidation'].includes(request.state)
        && !permissionIsAutomaticallyReviewed(request, automaticApproval)) ?? [];
    const runningTaskCount = [...(child?.commandTasks ?? []), ...(child?.backgroundTasks ?? [])]
        .filter(work => ['running', 'cancel_requested', 'outcome_unknown'].includes(work.state)).length;
    const jumpToPending = (id?: string) => {
        const target = id ? contentRef.current?.querySelector<HTMLElement>(`[id="assistant-directory-${id}"]`)
            : contentRef.current?.querySelector<HTMLElement>('[data-assistant-pending]');
        target?.scrollIntoView({ block: 'center', behavior: 'smooth' }); target?.focus({ preventScroll: true });
    };
    const detailsContent = <div className="space-y-4">
                    {task && <>
                        <Badge variant="outline">{t(`pages.aiAssistant.subagents.states.${task.state}`)}</Badge>
                        <p className="text-xs text-muted-foreground">{t(`pages.aiAssistant.subagents.sources.${task.source.kind}`)} · {formatLocalTime(task.updated_at)}</p>
                        {task.source.kind === 'user_input' && <p className="text-xs text-muted-foreground">
                            {t('pages.aiAssistant.subagents.sourceInput', { revision: task.source.input_revision })}
                        </p>}
                        {task.source.kind === 'goal' && <p className="break-all text-xs text-muted-foreground">{task.source.goal_id}</p>}
                        {task.source.kind === 'scheduled_occurrence' && <p className="break-all text-xs text-muted-foreground">{task.source.schedule_id} · {task.source.occurrence_id}</p>}
                        {task.wait_reason && <p className="text-sm">{t(`pages.aiAssistant.subagents.waitReasons.${task.wait_reason}`)}</p>}
                        <Disclosure title={t('pages.aiAssistant.subagents.identity')} summaryClassName="text-xs text-muted-foreground">
                            <p className="mt-2 break-all text-xs">{task.task_id} · {task.group_id}</p>
                        </Disclosure>
                    </>}
                    {agents.detail && <Disclosure title={t('pages.aiAssistant.subagents.objective')} defaultOpen summaryClassName="text-sm font-medium">
                        <p className="mt-2 whitespace-pre-wrap break-words text-sm">{agents.detail.result.objective}</p>
                        <ul className="mt-2 list-disc space-y-1 pl-5 text-xs text-muted-foreground">
                            {agents.detail.result.acceptance_criteria.map((criterion, index) => <li key={index}>{criterion}</li>)}
                        </ul>
                    </Disclosure>}
                    {report && <section className="space-y-2">
                        <p className="text-sm font-medium">{t('pages.aiAssistant.subagents.result')}</p>
                        <MarkdownContent disableLinks>{report.summary}</MarkdownContent>
                        {(['findings', 'delivered', 'remaining'] as const).map(section => report[section].length > 0 && <div key={section}>
                            <p className="text-xs font-medium">{t(`pages.aiAssistant.subagents.${section}`)}</p>
                            <ul className="list-disc space-y-1 pl-5 text-sm">{report[section].map((item, index) => <li key={index} className="whitespace-pre-wrap break-words">{item}</li>)}</ul>
                        </div>)}
                        {report.reason && <p className="whitespace-pre-wrap break-words text-sm text-muted-foreground">{report.reason}</p>}
                        {(report.receipt_refs.length > 0 || report.evidence_refs.length > 0) && <Disclosure title={t('pages.aiAssistant.subagents.references')} summaryClassName="text-xs">
                            <p className="mt-2 break-all text-xs text-muted-foreground">{[...report.receipt_refs, ...report.evidence_refs].join(' · ')}</p>
                        </Disclosure>}
                    </section>}
                    {agents.detail?.result.failure_reason && <p className="whitespace-pre-wrap text-sm text-destructive">{agents.detail.result.failure_reason}</p>}
                        {!!child?.taskStatusProjection?.items.length && <Disclosure title={t('pages.aiAssistant.subagents.progress')} summaryClassName="text-sm">
                            <ul className="mt-2 list-disc space-y-2 pl-5 text-sm">
                                {child.taskStatusProjection.items.map(item => <li key={item.itemId} className="break-words">
                                    {item.description} · {t(`pages.aiAssistant.taskStatus.${item.status}`)}{item.note && <p className="text-xs text-muted-foreground">{item.note}</p>}
                                </li>)}
                            </ul>
                        </Disclosure>}
                        {!!child?.capabilityGrants?.length && <Disclosure title={t('pages.aiAssistant.grantTitle')} summaryClassName="text-sm">
                            {child.capabilityGrants.map(grant => <div key={grant.grantId} className="mt-2 space-y-2 rounded-md border p-2">
                                <p className="break-all text-xs">{grant.grantId}</p>
                                <p className="break-words text-xs">{permissionToolLabel(t, grant.toolName)} · {grant.operationScope.map(value => permissionOperationLabel(t, value)).join(', ')}</p>
                                {grant.revokedAtUnixMs == null && grant.expiresAtUnixMs > Date.now() && <Button type="button" size="sm" variant="outline"
                                    disabled={agents.updating || !canDecide || !connected} onClick={() => void agents.revokeGrant(grant.grantId)}>{t('pages.aiAssistant.grantRevoke')}</Button>}
                            </div>)}
                        </Disclosure>}

        <Button type="button" variant="outline" disabled={agents.detailLoading} onClick={() => agents.openDetail(task)}>
            {t('common.refresh')}
        </Button>
    </div>;
    return <div className="flex min-h-0 flex-1 flex-col gap-2">
        {(pendingDirectories.length + pendingPermissions.length > 0 || runningTaskCount > 0) && <div className="flex shrink-0 flex-wrap gap-2 text-xs">
            {pendingDirectories.length + pendingPermissions.length > 0 && <Button type="button" size="sm" variant="outline" className="border-amber-500/50"
                onClick={() => jumpToPending()}>{t('pages.aiAssistant.workspace.pendingCount', { count: pendingDirectories.length + pendingPermissions.length })}</Button>}
            {runningTaskCount > 0 && <Button type="button" size="sm" variant="ghost" onClick={() => setPanel('tasks')}>
                {t('pages.aiAssistant.tasks.title')} ({runningTaskCount})</Button>}
        </div>}
        <div className="relative min-h-0 flex-1">
            <div ref={scrollRef} onScroll={onScroll} className="assistant-scrollbar h-full overflow-y-auto overscroll-contain [overflow-wrap:anywhere]">
                <div ref={contentRef} className="mx-auto w-full max-w-[840px] space-y-4 pb-4">
                    {agents.error && <p role="alert" className="text-sm text-destructive">{agents.error}</p>}
                    {agents.detailLoading && <p role="status" className="text-sm text-muted-foreground">{t('pages.aiAssistant.subagents.loading')}</p>}
                    {child && <>
                        {agents.hasMoreMessages && <Button type="button" size="sm" variant="ghost" disabled={agents.historyLoading}
                            onClick={() => void agents.loadOlderMessages()}>{t('pages.aiAssistant.loadEarlierMessages')}</Button>}
                        <AssistantTranscript sessionId={child.sessionId} messages={agents.childMessages} tools={agents.childTools}
                            running={child.active} evidence={child.visualEvidence ?? []} notices={child.contextNotices ?? []}
                            exportBackup={exportBackup && child.sessionId ? id => exportBackup(child.sessionId!, id) : undefined} />
                        {pendingDirectories.map(directory => <div key={`${child.sessionId}:${directory.requestId}`} data-assistant-pending tabIndex={-1}>
                            <AssistantDirectoryApproval directory={directory} revision={child.fileScope!.revision}
                                disabled={!canDecide || !connected || child.active || subagentIsTerminal(task)}
                                busy={agents.updating} onUpdate={agents.updateDirectory} />
                        </div>)}
                        <div data-assistant-pending={pendingPermissions.length > 0 ? '' : undefined} tabIndex={-1}>
                        <AssistantPermissionRecords key={child.sessionId} requests={child.permissionRequests ?? []} automaticApproval={automaticApproval}
                            open={panel === 'permissions'} onOpenChange={open => setPanel(open ? 'permissions' : null)}>
                            {permission => <AssistantPermissionRequest key={`${child.sessionId}:${permission.requestId}:${permission.inputRevision}`}
                                request={permission} canDecide={canDecide && !subagentIsTerminal(task) && permission.inputRevision === child.inputRevision}
                                disabled={!connected || child.active || agents.updating} busy={agents.updating} waitingForTurn={child.active}
                                interactive={interactive ? { ...interactive, runId: child.sessionId } : undefined} onDecide={agents.decidePermission} />}
                        </AssistantPermissionRecords>
                        </div>
                        {child.terminalError && <p role="alert" className="text-sm text-destructive">{child.terminalError.message}</p>}
                    </>}
                </div>
            </div>
            {showJumpToLatest && <Button type="button" variant="outline" size="icon" onClick={jumpToLatest}
                className="assistant-jump-action absolute bottom-3 right-3 rounded-full bg-background shadow-md"
                aria-label={t('pages.aiAssistant.scrollToLatest')}><ArrowDown className="h-4 w-4" /></Button>}
        </div>
        <div className="assistant-composer mx-auto w-full max-w-[840px] shrink-0 space-y-2 rounded-xl border bg-background p-3 shadow-sm">
            <AssistantSubagentApprovalNotice agents={agents} />
            {!subagentIsTerminal(task) && <form className="space-y-2" onSubmit={event => {
                event.preventDefault();
                if (validAdjustment && canDecide && connected) void agents.control(task, { kind: 'adjust', message: adjustment.trim() }).then(ok => {
                    if (ok) setAdjustment('');
                });
            }}>
                <Textarea aria-label={t('pages.aiAssistant.subagents.adjustMessage')} placeholder={t('pages.aiAssistant.subagents.adjustMessage')}
                    value={adjustment} maxLength={16_384} disabled={!canDecide || !connected || agents.updating}
                    onChange={event => setAdjustment(event.target.value)} className="min-h-16 max-h-40 resize-y" />
                <div className="flex flex-wrap items-center justify-between gap-2">
                    <AssistantComposerTools meter={<AssistantContextMeter usage={child?.contextUsage ?? null} draft={adjustment} />}
                        onDetails={() => setPanel('details')} onPermissionHistory={() => setPanel('permissions')}
                        onDirectories={() => setPanel('directories')} onTasks={() => setPanel('tasks')} runningTaskCount={runningTaskCount} />
                    <div className="flex items-center gap-2">
                        <Button type="button" variant="outline" disabled={!canDecide || !connected || agents.updating}
                            onClick={() => void agents.control(task, { kind: 'cancel' })}>{t('pages.aiAssistant.subagents.cancel')}</Button>
                        <Button type="submit" disabled={!validAdjustment || !canDecide || !connected || agents.updating}>
                            {t('pages.aiAssistant.subagents.applyAdjustment')}</Button>
                    </div>
                </div>
            </form>}
        </div>
        <AssistantDetailsSheet panel={panel === 'details' || panel === 'context' || panel === 'capabilities' ? panel : null}
            onPanelChange={next => setPanel(next === 'details' || next === 'context' || next === 'capabilities' ? next : null)} sections={{ details: detailsContent, ...(capabilityList ? { capabilities: capabilityList } : {}), context: child ? <div className="space-y-3">
                        {!!child.contextAttachments?.length && <Disclosure title={t('pages.aiAssistant.attachmentTitle')} defaultOpen summaryClassName="text-sm">
                            {child.contextAttachments.map(attachment => <div key={`${child.sessionId}:${attachment.id}`} className="mt-2 space-y-1 rounded-md border p-2">
                                <p className="break-words text-xs">{attachment.displaySummary}</p>
                                <p className="text-xs text-muted-foreground">{attachment.providerId} · {attachment.kind}</p>
                                <Badge variant={attachment.state === 'active' ? 'secondary' : 'outline'}>
                                    {attachment.state === 'active' ? t('pages.aiAssistant.attachmentActive')
                                        : t('pages.aiAssistant.attachmentStale', { reason: attachment.staleReason ?? attachment.state })}
                                </Badge>
                            </div>)}
                        </Disclosure>}

            {!child.contextAttachments?.length && <p className="text-sm text-muted-foreground">{t('pages.aiAssistant.contextEmpty')}</p>}
        </div> : null }} />
        {child && <>
            <AssistantAttachments key={`attachments:${child.sessionId}`} sessionId={child.sessionId} open={panel === 'attachments'}
                onOpenChange={open => setPanel(open ? 'attachments' : null)} showTrigger={false} />
            <AssistantFileScope key={`directories:${child.sessionId}`} scope={child.fileScope ?? { revision: 0, directories: [] }}
                deskId={deskId} sessionTargetId={sessionTargetId} open={panel === 'directories'}
                onOpenChange={open => setPanel(open ? 'directories' : null)}
                disabled={!canDecide || !connected || agents.updating} selectionDisabled={child.active || subagentIsTerminal(task)}
                onUpdate={agents.updateDirectory} showPendingActions={false} onPendingJump={jumpToPending} />
            <AssistantBackgroundTasks key={`tasks:${child.sessionId}`} open={panel === 'tasks'}
                onOpenChange={open => setPanel(open ? 'tasks' : null)} commands={child.commandTasks ?? []}
                providers={child.backgroundTasks ?? []} tools={agents.childTools} connected={canDecide && connected}
                canCancelProvider={canDecide && connected} cancelling={agents.updating ? 'busy' : null}
                onCancel={async (kind, id) => {
                    if (!await (kind === 'command' ? agents.cancelCommandTask(id) : agents.cancelProviderTask(id))) throw new Error('Cancellation failed');
                }} />
        </>}
    </div>;
}

export function AssistantConversationTabs({ agents, attentionTasks = [], mainNeedsApproval = false }: {
    agents: Pick<ReturnType<typeof useAiAssistantSubagents>, 'tasks' | 'selected' | 'openDetail' | 'closeDetail' | 'hasMore' | 'loading' | 'loadMore'> & Partial<Pick<ReturnType<typeof useAiAssistantSubagents>, 'detail'>>;
    attentionTasks?: { task_id: string }[];
    mainNeedsApproval?: boolean;
}) {
    const { t } = useTranslation();
    const unread = new Set(attentionTasks.map(task => task.task_id));
    const needsApproval = (task: typeof agents.tasks[number]) => !subagentIsTerminal(task) && (task.state === 'waiting_approval'
        || (agents.detail?.result.task.task_id === task.task_id && (agents.detail.session.permissionRequests?.some(request => ['pending', 'needs_revalidation'].includes(request.state))
            || agents.detail.session.fileScope?.directories.some(directory => directory.state === 'pending'))));
    const priority = (task: typeof agents.tasks[number]) => needsApproval(task) ? 0 : subagentIsTerminal(task) ? 2 : 1;
    const tasks = [...agents.tasks].sort((a, b) => priority(a) - priority(b)
        || a.created_at.localeCompare(b.created_at) || a.task_id.localeCompare(b.task_id));
    const viewport = useRef<HTMLDivElement>(null);
    const [scroll, setScroll] = useState({ previous: false, next: false });
    const revealSelected = useCallback(() => {
        const element = viewport.current;
        const selected = element?.querySelector<HTMLElement>('[aria-selected="true"]');
        if (!element || !selected) return;
        selected.scrollIntoView?.({ block: 'nearest', inline: selected.offsetWidth > element.clientWidth ? 'start' : 'nearest' });
    }, []);
    const measure = useCallback(() => {
        const element = viewport.current;
        if (!element) return;
        const previous = element.scrollLeft > 1;
        const next = element.scrollLeft + element.clientWidth < element.scrollWidth - 1;
        setScroll(current => current.previous === previous && current.next === next ? current : { previous, next });
    }, []);
    const hasTabs = tasks.length > 0 || agents.hasMore || mainNeedsApproval;
    const order = tasks.map(task => task.task_id).join('|');
    const layout = tasks.map(task => `${task.task_id}:${task.name}:${task.state}:${needsApproval(task)}`).join('|');
    const selectedId = agents.selected?.task_id;
    useEffect(() => {
        const element = viewport.current;
        if (!element) return;
        measure();
        const observer = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(() => { revealSelected(); measure(); });
        observer?.observe(element);
        element.querySelectorAll('[role="tab"]').forEach(tab => observer?.observe(tab));
        window.addEventListener('resize', measure);
        const wheel = (event: WheelEvent) => {
            if (Math.abs(event.deltaY) <= Math.abs(event.deltaX) || event.shiftKey) return;
            const next = Math.max(0, Math.min(element.scrollWidth - element.clientWidth, element.scrollLeft + event.deltaY));
            if (next === element.scrollLeft) return;
            event.preventDefault(); element.scrollLeft = next; measure();
        };
        element.addEventListener('wheel', wheel, { passive: false });
        return () => { observer?.disconnect(); window.removeEventListener('resize', measure); element.removeEventListener('wheel', wheel); };
    }, [hasTabs, layout, measure, revealSelected]);
    useEffect(() => {
        revealSelected(); measure();
    }, [selectedId, order, measure, revealSelected]);
    const move = (direction: number) => {
        const element = viewport.current;
        if (!element) return;
        const reducedMotion = window.matchMedia?.('(prefers-reduced-motion: reduce)').matches;
        element.scrollBy({ left: direction * Math.max(80, element.clientWidth * 0.8), behavior: reducedMotion ? 'auto' : 'smooth' });
    };
    if (!hasTabs) return null;
    return <div role="tablist" onKeyDown={event => {
        if (!(event.target instanceof HTMLElement) || event.target.getAttribute('role') !== 'tab'
            || !['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
        const tabs = Array.from(event.currentTarget.querySelectorAll<HTMLButtonElement>('[role="tab"]'));
        const index = tabs.indexOf(document.activeElement as HTMLButtonElement);
        const next = event.key === 'Home' ? 0 : event.key === 'End' ? tabs.length - 1
            : (Math.max(index, 0) + (event.key === 'ArrowRight' ? 1 : -1) + tabs.length) % tabs.length;
        event.preventDefault(); tabs[next]?.focus(); tabs[next]?.scrollIntoView?.({ block: 'nearest', inline: 'nearest' }); tabs[next]?.click();
    }} aria-label={t('pages.aiAssistant.subagents.conversationTabs')}
        className="assistant-conversation-tabs flex w-full max-w-full min-w-0 shrink-0 items-center gap-1 border-t pt-2">
        <Button type="button" role="tab" id="assistant-tab-main" aria-controls="assistant-main-panel"
            aria-selected={!agents.selected} tabIndex={agents.selected ? -1 : 0} variant={!agents.selected ? 'secondary' : 'ghost'}
            className="max-w-28 shrink-0" onClick={agents.closeDetail}>
            <span className={`min-w-0 truncate${mainNeedsApproval ? ' assistant-approval-tab-title' : ''}`}
                data-approval-pending={mainNeedsApproval || undefined}>{t('pages.aiAssistant.subagents.mainConversation')}</span>
            {mainNeedsApproval && <span className="sr-only">{t('pages.aiAssistant.subagents.states.waiting_approval')}</span>}
        </Button>
        {(scroll.previous || scroll.next) && <Button type="button" variant="ghost" size="icon" className="assistant-tab-scroll-button h-8 w-8 shrink-0"
            aria-label={t('pages.aiAssistant.subagents.scrollPrevious')} disabled={!scroll.previous} onClick={() => move(-1)}>
            <ChevronLeft className="h-4 w-4" aria-hidden="true" />
        </Button>}
        <div ref={viewport} data-testid="assistant-tab-viewport" onScroll={measure}
            className="assistant-scrollbar flex min-w-0 flex-1 items-center gap-1 overflow-x-auto pb-1">
        {tasks.map(task => <Button key={task.task_id} type="button" role="tab" id={`assistant-tab-${task.task_id}`}
            aria-controls="assistant-child-panel" aria-selected={agents.selected?.task_id === task.task_id}
            tabIndex={agents.selected?.task_id === task.task_id ? 0 : -1}
            variant={agents.selected?.task_id === task.task_id ? 'secondary' : 'ghost'} className="h-auto max-w-64 shrink-0 gap-2"
            onClick={() => { if (agents.selected?.task_id !== task.task_id) agents.openDetail(task); }}>
            <ClipboardList className="h-4 w-4 shrink-0" aria-hidden="true" />
            <span className={`min-w-0 truncate${needsApproval(task) ? ' assistant-approval-tab-title' : ''}`}
                data-approval-pending={needsApproval(task) || undefined}>{task.name}</span>
            {needsApproval(task) && task.state !== 'waiting_approval' && <span className="sr-only">{t('pages.aiAssistant.subagents.states.waiting_approval')}</span>}
            <Badge variant={task.state === 'waiting_approval' ? 'secondary' : 'outline'}>{t(`pages.aiAssistant.subagents.states.${task.state}`)}</Badge>
            {unread.has(task.task_id) && <span aria-label={t('pages.aiAssistant.subagents.unread')} className="h-2 w-2 shrink-0 rounded-full bg-primary" />}
        </Button>)}
        </div>
        {(scroll.previous || scroll.next) && <Button type="button" variant="ghost" size="icon" className="assistant-tab-scroll-button h-8 w-8 shrink-0"
            aria-label={t('pages.aiAssistant.subagents.scrollNext')} disabled={!scroll.next} onClick={() => move(1)}>
            <ChevronRight className="h-4 w-4" aria-hidden="true" />
        </Button>}
        {tasks.length > 0 && <DropdownMenu><DropdownMenuTrigger asChild>
            <Button type="button" variant="ghost" size="icon" className="h-9 w-9 shrink-0" aria-label={t('pages.aiAssistant.subagents.switchConversation')}>
                <ListFilter className="h-4 w-4" aria-hidden="true" />
            </Button>
        </DropdownMenuTrigger><DropdownMenuContent align="end" className="max-h-80 max-w-[calc(100vw-2rem)] overflow-y-auto">
            <DropdownMenuItem onSelect={agents.closeDetail}>{t('pages.aiAssistant.subagents.mainConversation')}</DropdownMenuItem>
            {tasks.map(task => <DropdownMenuItem key={task.task_id} onSelect={() => { if (selectedId !== task.task_id) agents.openDetail(task); }}>
                <span className={needsApproval(task) ? 'assistant-approval-tab-title' : undefined}>{task.name}</span>
                <span className="ml-auto pl-2 text-xs text-muted-foreground">{t(`pages.aiAssistant.subagents.states.${task.state}`)}</span>
            </DropdownMenuItem>)}
        </DropdownMenuContent></DropdownMenu>}
        {agents.hasMore && <Button type="button" variant="ghost" size="icon" className="h-9 w-9 shrink-0" disabled={agents.loading}
            aria-label={t('pages.aiAssistant.subagents.more')} title={t('pages.aiAssistant.subagents.more')}
            onClick={() => void agents.loadMore()}><Plus className="h-4 w-4" aria-hidden="true" /></Button>}
    </div>;
}
