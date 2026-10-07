import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { v4 } from 'uuid';
import { deskErrorCodeEnum } from '@/services/types';
import type { AiAssistantDelegationSnapshot, AiAssistantSubAgentControl, AiAssistantSubAgentPage,
    AiAssistantSubAgentResult, AiAssistantSubAgentSummary, PermissionDecisionBody, PermissionRequestDto } from '@/services/types';
import { projectPersistedSnapshot, type PersistedSnapshot } from './use-ai-assistant-chat';
import type { AssistantDirectoryOperation } from './assistant-file-scope';
import { prependSubagentHistory, refreshSubagentHistory, sameSubagentHistoryScope, type SubagentHistory } from './assistant-subagent-history';

export const subagentIsTerminal = (task: AiAssistantSubAgentSummary) =>
    ['completed', 'failed', 'cancelled'].includes(task.state);

async function request<T>(url: string, body?: unknown): Promise<T> {
    const controller = new AbortController();
    const timer = window.setTimeout(() => controller.abort(), 15_000);
    try {
        const response = await fetch(url, { credentials: 'include', signal: controller.signal,
            method: body === undefined ? 'GET' : 'POST',
            headers: { Accept: 'application/json', ...(body === undefined ? {} : { 'Content-Type': 'application/json' }) },
            ...(body === undefined ? {} : { body: JSON.stringify(body) }) });
        const result = await response.json();
        if (!response.ok || result.success !== true || result.code !== deskErrorCodeEnum.SUCCESS || result.data == null) {
            throw new Error(result.message || 'Subagent state unavailable');
        }
        return result.data as T;
    } finally { window.clearTimeout(timer); }
}

function mergeTasks(...sets: AiAssistantSubAgentSummary[][]) {
    const tasks = new Map<string, AiAssistantSubAgentSummary>();
    for (const set of sets) for (const task of set) {
        const prior = tasks.get(task.task_id);
        if (!prior || prior.state_revision <= task.state_revision) tasks.set(task.task_id, task);
    }
    return [...tasks.values()].sort((a, b) => Number(subagentIsTerminal(a)) - Number(subagentIsTerminal(b))
        || b.created_at.localeCompare(a.created_at) || b.task_id.localeCompare(a.task_id));
}

/** Child reads use their original session. Owner controls always target the root and task fences. */
export function useAiAssistantSubagents({ connection, conversation, session, snapshot, connected,
    onChanged }: { connection: string; conversation: string | null; session?: string;
    snapshot: AiAssistantDelegationSnapshot; connected: boolean; onChanged: () => void }) {
    const { t } = useTranslation();
    // New parent input does not replace the original child session or its controls.
    const scope = JSON.stringify([connection, session ?? '']);
    const currentScope = useRef(scope);
    currentScope.current = scope;
    const [dataScope, setDataScope] = useState(scope);
    const [page, setPage] = useState<AiAssistantSubAgentPage | null>(null);
    const [olderTasks, setOlderTasks] = useState<AiAssistantSubAgentSummary[]>([]);
    const [selected, setSelected] = useState<AiAssistantSubAgentSummary | null>(null);
    const [detail, setDetail] = useState<{ result: AiAssistantSubAgentResult; session: PersistedSnapshot } | null>(null);
    const detailRef = useRef<typeof detail>(null);
    const [history, setHistory] = useState<SubagentHistory | null>(null);
    const historyRef = useRef<SubagentHistory | null>(null);
    const historyOperation = useRef<object | null>(null);
    const selectionEpoch = useRef(0);
    const [historyLoading, setHistoryLoading] = useState(false);
    const selectedId = useRef<string | null>(null);
    selectedId.current = selected?.task_id ?? null;
    const [loading, setLoading] = useState(false);
    const [detailLoading, setDetailLoading] = useState(false);
    const [updating, setUpdating] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const operation = useRef<object | null>(null);
    const listOperation = useRef<object | null>(null);
    const detailOrder = useRef(0);
    const acceptedDetailOrder = useRef(0);
    const uiRead = useRef(new Set<string>());
    const listUrl = useCallback((path: string, extra?: Record<string, string>) => {
        const query = new URLSearchParams({ connection, session: session ?? '',
            ...(conversation ? { conversation } : {}), ...extra });
        return `/api/my/ai-assistant-session/subagents${path}?${query}`;
    }, [connection, session, conversation]);

    useEffect(() => {
        currentScope.current = scope; setDataScope(scope);
        setPage(null); setOlderTasks([]); setSelected(null); setDetail(null); setError(null);
        operation.current = null; listOperation.current = null;
        setLoading(false); setDetailLoading(false); setUpdating(false);
        detailOrder.current += 1; acceptedDetailOrder.current = detailOrder.current;
        uiRead.current.clear();
        selectionEpoch.current += 1; detailRef.current = null;
        historyRef.current = null; historyOperation.current = null; setHistory(null); setHistoryLoading(false);
        return () => { currentScope.current = ''; };
    }, [scope]);

    const tasks = useMemo(() => mergeTasks(olderTasks, page?.items ?? [], snapshot.tasks?.items ?? [],
        snapshot.active_tasks, snapshot.attention_tasks, detail ? [detail.result.task] : []), [olderTasks, page, snapshot, detail]);

    const loadMore = useCallback(async () => {
        if (!session || currentScope.current !== scope || dataScope !== scope || listOperation.current) return;
        const cursor = page?.next_cursor ?? snapshot.tasks?.next_cursor;
        if ((page ?? snapshot.tasks)?.has_more !== true || !cursor) return;
        const token = {}; listOperation.current = token; setLoading(true);
        try {
            const next = await request<AiAssistantSubAgentPage>(listUrl('', { cursor, limit: '20' }));
            if (currentScope.current !== scope || listOperation.current !== token) return;
            setOlderTasks(prior => mergeTasks(prior, page?.items ?? [], next.items));
            setPage(next);
        } catch (reason) {
            if (currentScope.current === scope) setError(reason instanceof Error ? reason.message : t('pages.aiAssistant.subagents.readFailed'));
        } finally {
            if (currentScope.current === scope && listOperation.current === token) {
                listOperation.current = null; setLoading(false);
            }
        }
    }, [session, page, snapshot.tasks, listUrl, scope, dataScope, t]);

    const loadDetail = useCallback(async (task: AiAssistantSubAgentSummary, foreground = false) => {
        if (!session || currentScope.current !== scope || dataScope !== scope) return;
        const order = ++detailOrder.current;
        const epoch = selectionEpoch.current;
        if (foreground) setDetailLoading(true);
        try {
            const [result, child] = await Promise.all([
                request<AiAssistantSubAgentResult>(listUrl('/result', { task_id: task.task_id })),
                request<PersistedSnapshot>(`/api/my/ai-assistant-session?${new URLSearchParams({ connection,
                    session: task.child_session_id })}`),
            ]);
            if (currentScope.current !== scope || selectionEpoch.current !== epoch || selectedId.current !== task.task_id || order < acceptedDetailOrder.current) return;
            if (result.task.task_id !== task.task_id || result.task.child_session_id !== task.child_session_id
                || child.sessionId !== task.child_session_id || child.subagents.parent_session_id !== session
                || child.subagents.task?.task_id !== task.task_id || !Array.isArray(child.messages)
                || result.task.group_id !== task.group_id || result.task.state_revision < task.state_revision
                || child.inputRevision !== result.task.input_revision || child.controlRevision !== result.task.control_revision
                || !sameSubagentHistoryScope(child.subagents.task, result.task)
                || child.subagents.task.state_revision !== result.task.state_revision
                || child.subagents.task.input_revision !== result.task.input_revision
                || child.subagents.task.control_revision !== result.task.control_revision
                || child.subagents.task.group_id !== result.task.group_id || child.subagents.task.state !== result.task.state) {
                throw new Error(t('pages.aiAssistant.subagents.readFailed'));
            }
            const prior = detailRef.current;
            if (prior && (prior.session.seq > child.seq || prior.result.task.state_revision > result.task.state_revision)) return;
            acceptedDetailOrder.current = order;
            detailRef.current = { result, session: child };
            historyRef.current = refreshSubagentHistory(historyRef.current, child);
            setHistory(historyRef.current); setDetail(detailRef.current); setSelected(result.task);
        } catch (reason) {
            if (currentScope.current === scope && selectionEpoch.current === epoch && selectedId.current === task.task_id && foreground) {
                setError(reason instanceof Error ? reason.message : t('pages.aiAssistant.subagents.readFailed'));
            }
        } finally {
            if (currentScope.current === scope && selectionEpoch.current === epoch && selectedId.current === task.task_id && foreground) setDetailLoading(false);
        }
    }, [session, connection, listUrl, scope, dataScope, t]);

    const openDetail = useCallback((task: AiAssistantSubAgentSummary) => {
        if (currentScope.current !== scope || dataScope !== scope) return;
        selectionEpoch.current += 1; detailRef.current = null;
        historyRef.current = null; historyOperation.current = null; setHistory(null); setHistoryLoading(false);
        selectedId.current = task.task_id;
        setSelected(task); setDetail(null); setError(null);
        void loadDetail(task, true);
    }, [scope, dataScope, loadDetail]);
    const closeDetail = useCallback(() => {
        if (currentScope.current !== scope) return;
        selectionEpoch.current += 1; detailRef.current = null;
        historyRef.current = null; historyOperation.current = null; setHistory(null); setHistoryLoading(false);
        selectedId.current = null; setSelected(null); setDetail(null); setError(null);
    }, [scope]);

    const loadOlderMessages = useCallback(async () => {
        const current = detailRef.current;
        const window = historyRef.current;
        const cursor = window?.nextBefore;
        if (currentScope.current !== scope || dataScope !== scope || !session || !current || !window?.hasMore
            || !cursor || historyOperation.current) return;
        const task = current.result.task;
        const epoch = selectionEpoch.current;
        const token = {}; historyOperation.current = token; setHistoryLoading(true);
        const isCurrent = () => currentScope.current === scope && selectionEpoch.current === epoch
            && historyOperation.current === token && selectedId.current === task.task_id;
        try {
            const page = await request<PersistedSnapshot>(`/api/my/ai-assistant-session?${new URLSearchParams({
                connection, session: task.child_session_id, message_before: cursor, message_limit: '100' })}`);
            if (!isCurrent()) return;
            const latest = detailRef.current;
            const prior = historyRef.current;
            if (!latest || !prior || prior.nextBefore !== cursor || !sameSubagentHistoryScope(latest.result.task, task)
                || page.sessionId !== task.child_session_id || page.subagents.parent_session_id !== session
                || !page.subagents.task || !sameSubagentHistoryScope(page.subagents.task, task)
                || page.inputRevision !== task.input_revision || page.controlRevision !== task.control_revision
                || !Array.isArray(page.messages) || !Number.isSafeInteger(page.seq) || page.seq < current.session.seq) return;
            historyRef.current = prependSubagentHistory(prior, page); setHistory(historyRef.current);
        } catch (reason) {
            if (isCurrent()) setError(reason instanceof Error ? reason.message : t('pages.aiAssistant.subagents.readFailed'));
        } finally { if (isCurrent()) { historyOperation.current = null; setHistoryLoading(false); } }
    }, [scope, dataScope, session, connection, t]);

    useEffect(() => {
        if (!selected) return;
        const timer = window.setInterval(() => { void loadDetail(selected); }, 2_000);
        return () => window.clearInterval(timer);
    }, [selected?.task_id, loadDetail]);

    useEffect(() => {
        if (!session || currentScope.current !== scope || dataScope !== scope || !detail || selectedId.current !== detail.result.task.task_id) return;
        const task = detail.result.task;
        const key = `${scope}/${task.task_id}/${task.state_revision}`;
        if (uiRead.current.has(key)) return;
        uiRead.current.add(key);
        void request('/api/my/ai-assistant-session/subagents/read', { connection, conversation, session,
            task_id: task.task_id, through_state_revision: task.state_revision }).then(() => {
            if (currentScope.current === scope) onChanged();
        }).catch(() => { if (currentScope.current === scope) uiRead.current.delete(key); });
    }, [detail, session, scope, dataScope, connection, conversation, onChanged]);

    // A formerly active child can finish outside the recent page. Refresh that
    // exact task instead of retaining a stale running entry or guessing its result.
    useEffect(() => {
        const active = new Set(snapshot.active_tasks.map(task => task.task_id));
        const missing = mergeTasks(olderTasks, page?.items ?? []).filter(task => !subagentIsTerminal(task) && !active.has(task.task_id)).slice(0, 2);
        if (!missing.length || !session) return;
        let cancelled = false;
        void Promise.allSettled(missing.map(task => request<AiAssistantSubAgentSummary>(listUrl('/status', { task_id: task.task_id })))).then(results => {
            if (cancelled || currentScope.current !== scope) return;
            const current = results.flatMap(result => result.status === 'fulfilled' ? [result.value] : []);
            if (current.length) setOlderTasks(prior => current.some(task => {
                const existing = prior.find(item => item.task_id === task.task_id)
                    ?? page?.items.find(item => item.task_id === task.task_id);
                return !existing || existing.state_revision < task.state_revision;
            }) ? mergeTasks(prior, current) : prior);
        });
        return () => { cancelled = true; };
    }, [snapshot, session, page, olderTasks, listUrl, scope]);

    const control = useCallback(async (task: AiAssistantSubAgentSummary, action: AiAssistantSubAgentControl['action']) => {
        if (!session || currentScope.current !== scope || dataScope !== scope || operation.current || subagentIsTerminal(task)) return false;
        const epoch = selectionEpoch.current;
        const token = {}; operation.current = token; setUpdating(true); setError(null);
        const isCurrent = () => currentScope.current === scope && operation.current === token && selectionEpoch.current === epoch;
        try {
            const value: AiAssistantSubAgentControl = { task_id: task.task_id, client_request_id: v4(),
                expected_input_revision: task.input_revision, expected_control_revision: task.control_revision, action };
            const next = await request<AiAssistantSubAgentSummary>('/api/my/ai-assistant-session/subagents/control',
                { connection, conversation, session, control: value });
            if (!isCurrent()) return false;
            if (next.task_id !== task.task_id || next.child_session_id !== task.child_session_id || next.group_id !== task.group_id
                || next.control_revision < task.control_revision || next.input_revision < task.input_revision) {
                throw new Error(t('pages.aiAssistant.subagents.controlFailed'));
            }
            setOlderTasks(prior => mergeTasks(prior, [next]));
            onChanged();
            if (selectedId.current === task.task_id) await loadDetail(next, true);
            return isCurrent();
        } catch (reason) {
            if (isCurrent()) {
                onChanged();
                if (selectedId.current === task.task_id) await loadDetail(task, true);
                if (isCurrent()) setError(reason instanceof Error ? reason.message : t('pages.aiAssistant.subagents.controlFailed'));
            }
            return false;
        } finally { if (currentScope.current === scope && operation.current === token) { operation.current = null; setUpdating(false); } }
    }, [session, scope, dataScope, connection, conversation, onChanged, loadDetail, t]);

    const decidePermission = useCallback(async (permission: PermissionRequestDto, items: PermissionDecisionBody['items'], carrierId?: string) => {
        const current = detail;
        if (currentScope.current !== scope || dataScope !== scope || !current || !selected || !connected || operation.current || current.session.active
            || subagentIsTerminal(current.result.task) || permission.inputRevision !== current.session.inputRevision
            || !current.session.permissionRequests?.some(item => item.requestId === permission.requestId && item.state === 'pending'
                && item.inputRevision === permission.inputRevision)) return false;
        const epoch = selectionEpoch.current;
        const token = {}; operation.current = token; setUpdating(true); setError(null);
        const isCurrent = () => currentScope.current === scope && selectedId.current === selected.task_id && operation.current === token
            && selectionEpoch.current === epoch;
        try {
            const body: PermissionDecisionBody = { connection, session: current.session.sessionId,
                requestId: permission.requestId, items, expectedRunRequestId: current.session.requestId,
                ...(carrierId ? { carrierId } : {}) };
            await request('/api/my/ai-assistant-session/permission-decision', body);
            if (!isCurrent()) return false;
            await loadDetail(selected, true); onChanged();
            return isCurrent();
        } catch (reason) {
            if (isCurrent()) {
                await loadDetail(selected, true);
                if (isCurrent()) setError(reason instanceof Error ? reason.message : t('pages.aiAssistant.subagents.controlFailed'));
            }
            return false;
        } finally { if (currentScope.current === scope && operation.current === token) { operation.current = null; setUpdating(false); } }
    }, [detail, selected, connected, connection, scope, dataScope, loadDetail, onChanged, t]);

    const mutateChild = useCallback(async (path: string, body: unknown) => {
        if (currentScope.current !== scope || dataScope !== scope || !detail || !selected || operation.current) return false;
        const original = selected;
        const epoch = selectionEpoch.current;
        const token = {}; operation.current = token; setUpdating(true); setError(null);
        const isCurrent = () => currentScope.current === scope && operation.current === token && selectionEpoch.current === epoch;
        try {
            await request(`/api/my/ai-assistant-session/${path}`, body);
            if (!isCurrent()) return false;
            await loadDetail(original, true); onChanged(); return isCurrent();
        } catch (reason) {
            if (isCurrent()) {
                await loadDetail(original, true); onChanged();
                if (isCurrent() && selectedId.current === original.task_id) setError(reason instanceof Error ? reason.message
                    : t('pages.aiAssistant.subagents.controlFailed'));
            }
            return false;
        } finally { if (currentScope.current === scope && operation.current === token) { operation.current = null; setUpdating(false); } }
    }, [detail, selected, scope, dataScope, loadDetail, onChanged, t]);

    const updateDirectory = useCallback((action: AssistantDirectoryOperation, _timeoutMessage: string) => {
        if (currentScope.current !== scope || dataScope !== scope || !detail || !selected || operation.current
            || action.expected_revision !== detail.session.fileScope?.revision) return false;
        if (action.kind === 'select_directory') {
            if (!connected || detail.session.active || subagentIsTerminal(detail.result.task) || !action.path.trim()
                || new TextEncoder().encode(action.path).length > 4096 || !action.purpose.trim()
                || new TextEncoder().encode(action.purpose).length > 2048 || /[\u0000-\u001f\u007f-\u009f]/.test(action.path + action.purpose)) return false;
        } else if (!detail.session.fileScope.directories.some(item => item.requestId === action.directory_request_id)) return false;
        if (action.kind === 'decide_directory' && action.approve && (!connected || detail.session.active || subagentIsTerminal(detail.result.task))) return false;
        void mutateChild('directory-control', { connection, session: detail.session.sessionId,
            client_request_id: v4(), operation: action });
        return true;
    }, [detail, selected, connected, connection, scope, dataScope, mutateChild]);

    const revokeGrant = useCallback((grantId: string) => {
        if (!detail?.session.capabilityGrants?.some(grant => grant.grantId === grantId)) return Promise.resolve(false);
        return mutateChild('capability-grant/revoke', { connection, session: detail.session.sessionId,
            grantId, reason: 'revoked_by_owner' });
    }, [detail, connection, mutateChild]);

    const cancelProviderTask = useCallback((taskId: string) => {
        if (!detail?.session.backgroundTasks?.some(task => task.taskId === taskId && task.supportsCancel
            && ['running', 'outcome_unknown'].includes(task.state))) return Promise.resolve(false);
        return mutateChild('background-task/cancel', { connection, session: detail.session.sessionId,
            taskId, requestId: v4(), reason: 'Cancelled by the conversation owner.' });
    }, [detail, connection, mutateChild]);

    const cancelCommandTask = useCallback((taskId: string) => {
        const task = detail?.session.commandTasks?.find(item => item.taskId === taskId
            && ['running', 'outcome_unknown'].includes(item.state));
        if (!task) return Promise.resolve(false);
        return mutateChild('command/cancel', { connection, session: detail!.session.sessionId,
            exec_request_id: task.taskId, execution_generation: task.executionGeneration });
    }, [detail, connection, mutateChild]);

    const visible = dataScope === scope && !!session;
    return { tasks: visible ? tasks : [], total: visible ? snapshot.tasks?.total ?? page?.total ?? tasks.length : 0,
        attentionCount: visible ? snapshot.attention_count : 0,
        unfinished: visible ? snapshot.tasks?.unfinished ?? snapshot.active_tasks.length : 0,
        hasMore: visible && ((page ?? snapshot.tasks)?.has_more ?? false), loading, loadMore,
        selected: visible ? selected : null, detail: visible ? detail : null, detailLoading, openDetail, closeDetail, updating, error: visible ? error : null, control, decidePermission,
        updateDirectory, revokeGrant, cancelProviderTask, cancelCommandTask,
        historyLoading, hasMoreMessages: visible && (history?.hasMore ?? false), loadOlderMessages,
        childMessages: visible && detail ? projectPersistedSnapshot({ ...detail.session, messages: history?.messages ?? detail.session.messages }).messages : [],
        childTools: visible && detail ? projectPersistedSnapshot({ ...detail.session, messages: history?.messages ?? detail.session.messages }).tools : [] };
}
