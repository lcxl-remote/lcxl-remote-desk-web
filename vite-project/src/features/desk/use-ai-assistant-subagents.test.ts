import { act, renderHook, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { AiAssistantDelegationSnapshot, AiAssistantSubAgentResult, AiAssistantSubAgentSummary, PermissionRequestDto } from '@/services/types';
import { deskErrorCodeEnum } from '@/services/types';
import { useAiAssistantSubagents } from './use-ai-assistant-subagents';
import type { PersistedSnapshot } from './use-ai-assistant-chat';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
const task: AiAssistantSubAgentSummary = { task_id: 'task-1', worker_id: "worker-1", child_session_id: 'child-1', group_id: 'group-1',
    name: 'Independent research', state: 'waiting_approval', input_revision: 1, control_revision: 1, state_revision: 3,
    source_goal_id: null, source: { kind: 'user_input', input_revision: 1 }, wait_reason: 'owner_approval',
    created_at: '2026-09-30T00:00:00Z', updated_at: '2026-09-30T00:00:01Z' };
const snapshot: AiAssistantDelegationSnapshot = { parent_session_id: null, task: null, active_tasks: [task],
    attention_tasks: [task], attention_count: 1,
    tasks: { items: [task], total: 1, unfinished: 1, next_cursor: null, has_more: false } };
const report: AiAssistantSubAgentResult = { task, objective: 'Read the original evidence',
    acceptance_criteria: ['Cite the evidence'], report: null, failure_reason: null,
    reports: { items: [], next_before_seq: null }, open_question: null };
const permission = { requestId: 'child-permission', inputRevision: 1, state: 'pending', items: [] } as unknown as PermissionRequestDto;
const child: PersistedSnapshot = { sessionId: 'child-1', seq: 2, active: false, requestId: 'child-request',
    inputRevision: 1, controlRevision: 1, mainStopped: false, messages: [], permissionRequests: [permission],
    subagents: { parent_session_id: 'root-1', task, active_tasks: [], tasks: null, attention_tasks: [], attention_count: 0 } };
const ok = (data: unknown) => ({ ok: true, json: async () => ({ success: true, code: deskErrorCodeEnum.SUCCESS, data }) });
const props = { connection: 'original-device', conversation: 'owner-conversation', session: 'root-1',
    snapshot, connected: true, onChanged: vi.fn() };

describe('owner subagent views', () => {
    afterEach(() => { vi.unstubAllGlobals(); vi.clearAllMocks(); });

    it('adds the sending client timezone to an owner adjustment', async () => {
        const fetch = vi.fn(async (_url: string, _init?: RequestInit) => ok(task));
        vi.stubGlobal('fetch', fetch);
        const { result, unmount } = renderHook(() => useAiAssistantSubagents(props));
        await act(async () => {
            expect(await result.current.control(task, { kind: 'adjust', message: '检查昨天的日志' })).toBe(true);
        });
        const submitted = fetch.mock.calls.find(([url]) => url.endsWith('/subagents/control'));
        expect(JSON.parse(submitted![1]!.body as string).control.action).toEqual({
            kind: 'adjust', message: '检查昨天的日志',
            client_timezone: Intl.DateTimeFormat().resolvedOptions().timeZone,
        });
        unmount();
    });

    it('loads earlier reports under the original request and keeps the live question for replies', async () => {
        const item = (seq: number) => ({ report_id: `report-${seq}`, seq, kind: 'progress' as const,
            summary: `Report ${seq}`, question: null, evidence_refs: [], receipt_refs: [] });
        const latest = { ...report, reports: { items: [item(2)], next_before_seq: 2 },
            open_question: { question_id: 'original-question', text: 'Which environment?' } };
        const fetch = vi.fn(async (url: string, _init?: RequestInit) => {
            if (url.includes('before_report_seq=2')) {
                return ok({ ...latest, reports: { items: [item(1)], next_before_seq: null },
                    open_question: { question_id: 'old-question', text: 'Obsolete question' } });
            }
            if (url.includes('/subagents/result')) return ok(latest);
            if (url.endsWith('/subagents/control')) return ok(task);
            if (url.endsWith('/subagents/read')) return ok(true);
            return ok(child);
        });
        vi.stubGlobal('fetch', fetch);
        const { result, unmount } = renderHook(() => useAiAssistantSubagents(props));
        act(() => result.current.openDetail(task));
        await waitFor(() => expect(result.current.reports?.items[0].seq).toBe(2));
        await act(async () => { await result.current.loadOlderReports(); });
        expect(result.current.reports?.items[0].seq).toBe(1);
        expect(result.current.detail?.result.open_question?.question_id).toBe('original-question');
        const earlier = fetch.mock.calls.find(([url]) => url.includes('before_report_seq=2'))?.[0];
        expect(earlier).toContain('session=root-1');
        expect(earlier).toContain('task_id=task-1');
        act(() => result.current.showLatestReports());
        expect(result.current.reports?.items[0].seq).toBe(2);
        expect(await result.current.control(task, { kind: 'reply', question_id: 'old-question', message: 'Ignore.' })).toBe(false);
        await act(async () => {
            expect(await result.current.control(task, { kind: 'reply', question_id: 'original-question', message: 'Staging.' })).toBe(true);
        });
        const submitted = fetch.mock.calls.find(([url]) => url.endsWith('/subagents/control'));
        expect(JSON.parse(submitted![1]!.body as string).control).toMatchObject({
            task_id: task.task_id, expected_input_revision: task.input_revision, expected_control_revision: task.control_revision,
            action: { kind: 'reply', question_id: 'original-question', message: 'Staging.' },
        });
        unmount();
    });

    it('submits long directory input intact and retains revision and control-character guards', async () => {
        const childWithScope = { ...child, fileScope: { revision: 7, directories: [] } };
        const fetch = vi.fn(async (url: string, _init?: RequestInit) => {
            if (url.includes('/subagents/result')) return ok(report);
            if (url.endsWith('/subagents/read') || url.endsWith('/directory-control')) return ok(true);
            return ok(childWithScope);
        });
        vi.stubGlobal('fetch', fetch);
        const { result, unmount } = renderHook(() => useAiAssistantSubagents(props));
        act(() => result.current.openDetail(task));
        await waitFor(() => expect(result.current.detail?.session.fileScope?.revision).toBe(7));
        const action = { kind: 'select_directory' as const, expected_revision: 7,
            path: '/tmp/' + '目录'.repeat(3000), purpose: '分析项目'.repeat(1500) };
        expect(result.current.updateDirectory({ ...action, expected_revision: 6 }, 'timeout')).toBe(false);
        expect(result.current.updateDirectory({ ...action, purpose: 'invalid\ncontrol' }, 'timeout')).toBe(false);
        act(() => { expect(result.current.updateDirectory(action, 'timeout')).toBe(true); });
        await waitFor(() => expect(fetch.mock.calls.some(([url]) => url.endsWith('/directory-control'))).toBe(true));
        const submitted = fetch.mock.calls.find(([url]) => url.endsWith('/directory-control'));
        const body = JSON.parse(submitted![1]!.body as string);
        expect(body.operation).toEqual(action);
        expect(body.connection).toBe(props.connection);
        expect(body.session).toBe(child.sessionId);
        await waitFor(() => expect(result.current.updating).toBe(false));
        unmount();
    });

    it('reads the original child and marks UI attention without changing task or model state', async () => {
        const fetch = vi.fn(async (url: string) => {
            if (url.includes('/subagents/result')) return ok(report);
            if (url.endsWith('/subagents/read')) return ok(true);
            return ok(child);
        }); vi.stubGlobal('fetch', fetch);
        const { result, unmount } = renderHook(() => useAiAssistantSubagents(props));
        act(() => result.current.openDetail(task));
        await waitFor(() => expect(result.current.detail?.session.sessionId).toBe('child-1'));
        await waitFor(() => expect(fetch.mock.calls.some(([url]) => url.endsWith('/subagents/read'))).toBe(true));
        expect(fetch.mock.calls.find(([url]) => url.startsWith('/api/my/ai-assistant-session?'))?.[0]).toContain('session=child-1');
        expect(result.current.detail?.result.task).toEqual(task);
        expect(result.current.detail?.session.messages).toEqual([]);
        expect(props.onChanged).toHaveBeenCalled();
        unmount();
    });

    it('submits child approval with its original request selector after the parent changes input', async () => {
        const fetch = vi.fn(async (url: string, _init?: RequestInit) => {
            if (url.includes('/subagents/result')) return ok(report);
            if (url.endsWith('/subagents/read') || url.endsWith('/permission-decision')) return ok(true);
            return ok(child);
        }); vi.stubGlobal('fetch', fetch);
        const { result, rerender, unmount } = renderHook(({ conversation }) => useAiAssistantSubagents({ ...props, conversation }),
            { initialProps: { conversation: 'owner-conversation' } });
        act(() => result.current.openDetail(task));
        await waitFor(() => expect(result.current.detail).not.toBeNull());
        rerender({ conversation: 'same-root-later-input' });
        expect(result.current.selected?.task_id).toBe(task.task_id);
        expect(result.current.detail?.session.sessionId).toBe('child-1');
        await act(async () => { expect(await result.current.decidePermission(permission, [])).toBe(true); });
        const submitted = fetch.mock.calls.find(([url]) => url.endsWith('/permission-decision'));
        expect(JSON.parse(submitted![1]!.body as string)).toEqual({ connection: 'original-device', session: 'child-1',
            requestId: 'child-permission', expectedRunRequestId: 'child-request', items: [] });
        unmount();
    });

    it('rejects a result and child snapshot taken across an adjustment', async () => {
        const changed = { ...task, input_revision: 2, control_revision: 2, state_revision: 4 };
        const fetch = vi.fn(async (url: string) => url.includes('/subagents/result') ? ok(report)
            : ok({ ...child, inputRevision: 2, controlRevision: 2, subagents: { ...child.subagents, task: changed } }));
        vi.stubGlobal('fetch', fetch);
        const { result, unmount } = renderHook(() => useAiAssistantSubagents(props));
        act(() => result.current.openDetail(task));
        await waitFor(() => expect(result.current.error).not.toBeNull());
        expect(result.current.detail).toBeNull();
        expect(fetch.mock.calls.some(([url]) => url.endsWith('/subagents/read'))).toBe(false);
        expect(await result.current.decidePermission(permission, [])).toBe(false);
        unmount();
    });

    it('requests cancellation through the original child native generation after parent input changes', async () => {
        const command = { taskId: 'original-command', callId: 'command-call', executionGeneration: 'original-generation',
            state: 'outcome_unknown' as const, updatedAt: '2026-09-30T00:00:02Z', result: null, resultTruncated: false };
        const fetch = vi.fn(async (url: string, _init?: RequestInit) => {
            if (url.includes('/subagents/result')) return ok(report);
            if (url.endsWith('/subagents/read') || url.endsWith('/command/cancel')) return ok(true);
            return ok({ ...child, commandTasks: [command] });
        }); vi.stubGlobal('fetch', fetch);
        const { result, rerender, unmount } = renderHook(({ conversation }) => useAiAssistantSubagents({ ...props, conversation }),
            { initialProps: { conversation: 'owner-conversation' } });
        act(() => result.current.openDetail(task));
        await waitFor(() => expect(result.current.detail).not.toBeNull());
        rerender({ conversation: 'later-parent-input' });
        await act(async () => { expect(await result.current.cancelCommandTask(command.taskId)).toBe(true); });
        const submitted = fetch.mock.calls.filter(([url]) => url.endsWith('/command/cancel'));
        expect(submitted).toHaveLength(1);
        expect(JSON.parse(submitted[0][1]!.body as string)).toEqual({ connection: 'original-device', session: 'child-1',
            exec_request_id: 'original-command', execution_generation: 'original-generation' });
        expect(await result.current.cancelCommandTask('another-command')).toBe(false);
        unmount();
    });

    it('ignores a late detail response and old control callbacks after switching roots', async () => {
        let release!: (value: ReturnType<typeof ok>) => void;
        const fetch = vi.fn(async (url: string) => url.includes('/subagents/result') ? ok(report)
            : new Promise<ReturnType<typeof ok>>(resolve => { release = resolve; }));
        vi.stubGlobal('fetch', fetch);
        const { result, rerender, unmount } = renderHook(({ session }) => useAiAssistantSubagents({ ...props, session }),
            { initialProps: { session: 'root-1' } });
        const oldControl = result.current.control;
        act(() => result.current.openDetail(task));
        rerender({ session: 'root-2' });
        await act(async () => { release(ok(child)); });
        expect(result.current.detail).toBeNull();
        expect(result.current.selected).toBeNull();
        expect(await oldControl(task, { kind: 'cancel' })).toBe(false);
        expect(fetch.mock.calls.some(([url]) => url.endsWith('/subagents/control'))).toBe(false);
        unmount();
    });

    it('keeps unread completed tasks visible when they are outside the recent page', () => {
        const older = { ...task, task_id: 'older-task', child_session_id: 'older-child', state: 'completed' as const,
            wait_reason: null, state_revision: 9 };
        const { result, unmount } = renderHook(() => useAiAssistantSubagents({ ...props, snapshot: {
            ...snapshot, active_tasks: [], tasks: { ...snapshot.tasks!, items: [], total: 40, unfinished: 0 },
            attention_tasks: [older], attention_count: 25 } }));
        expect(result.current.tasks.map(item => item.task_id)).toContain('older-task');
        expect(result.current.attentionCount).toBe(25);
        unmount();
    });
});
