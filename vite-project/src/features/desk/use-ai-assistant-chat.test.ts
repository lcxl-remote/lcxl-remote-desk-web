import { act, renderHook, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
    SIGNALING_TYPE_CODE_ASK_AI_ASSISTANT,
    SIGNALING_TYPE_CODE_CONTROL_EXECUTION,
    SIGNALING_TYPE_CODE_AI_ASSISTANT_CONTEXT_UPDATED,
    SIGNALING_TYPE_CODE_AI_ASSISTANT_OBJECT_CONTEXT_UPDATED,
    SIGNALING_TYPE_CODE_AI_ASSISTANT_SESSION_SELECTED,
    SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED,
    SIGNALING_TYPE_CODE_SELECT_AI_ASSISTANT_SESSION,
    SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_CONTEXT,
    SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_OBJECT_CONTEXT,
    SIGNALING_TYPE_CODE_QUERY_COMPUTER_ACTION_TURN,
    SIGNALING_TYPE_CODE_COMPUTER_ACTION_TURN_STATUS,
    SIGNALING_TYPE_CODE_REQUEST_DOCUMENT_PREVIEW_PAGE,
    SIGNALING_TYPE_CODE_DOCUMENT_PREVIEW_PAGE_UPDATED,
} from './constants';
import type { SignalingSubscriber } from './use-desk-signaling';
import { useAiAssistantChat } from './use-ai-assistant-chat';
import { deskErrorCodeEnum } from '@/services/types';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));

const snapshotFields = { controlRevision: 1, inputRevision: 1, mainStopped: false,
    subagents: { active_tasks: [], task: null, tasks: null, parent_session_id: null, attention_tasks: [], attention_count: 0 } };

function stubSnapshotFetch<Args extends unknown[]>(fetchSnapshot: (...args: Args) => unknown) {
    vi.stubGlobal('fetch', (...args: Args) => String(args[0]).includes('/ai-assistant-session/requests?')
        ? Promise.resolve({ ok: true, json: async () => ({ success: true, data: { sessionId: 'session', items: [] } }) })
        : fetchSnapshot(...args));
}

describe('useAiAssistantChat', () => {
    it('submits to a workbench queue without a root chat snapshot and recovers its receipt', async () => {
        let accepted: { clientMessageId: string; requestId: string; state: 'queued' } | undefined;
        const fetchMock = vi.fn(async (_url: string) => ({
            ok: true,
            json: async () => ({ success: true, data: { sessionId: 'workbench-session', items: accepted ? [accepted] : [] } }),
        }));
        vi.stubGlobal('fetch', fetchMock);
        const sendMessage = vi.fn((_type: number, _data: unknown, _to?: string, _request?: string): string => {
            throw new Error('Receipt lost');
        });
        const { result } = renderHook(() => useAiAssistantChat({
            deskId: '', initialConversationId: 'workbench-created', conversationStorageScope: 'workbench:created',
            queueOnly: true, targetRefs: ['mac'], subscribe: () => () => undefined, sendMessage,
        }));
        await waitFor(() => expect(fetchMock).toHaveBeenCalled());
        act(() => { expect(result.current.start('Reproduce the issue')).toBe(true); });
        const input = sendMessage.mock.calls[0][1] as { client_message_id: string };
        expect(input).toMatchObject({ conversation_id: 'workbench-created', target_refs: ['mac'] });
        accepted = { clientMessageId: input.client_message_id, requestId: 'durable-request', state: 'queued' };
        await act(async () => { await result.current.retryDelivery(); });
        expect(result.current.acceptedInput?.question).toBe('Reproduce the issue');
        expect(sendMessage).toHaveBeenCalledTimes(1);
        expect(fetchMock.mock.calls.every(([url]) => url.includes('/ai-assistant-session/requests?'))).toBe(true);
    });

    it('freezes issue selections with the original message when delivery is retried', async () => {
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: null }) }));
        const sendMessage = vi.fn((_type: number, _payload: unknown, _to?: string, request?: string) => request!)
            .mockImplementationOnce(() => { throw new Error('offline'); });
        const { result } = renderHook(() => useAiAssistantChat({ deskId: '', conversationStorageScope: 'personal:issue-sources',
            subscribe: () => () => undefined, sendMessage }));
        const issue = { connection_ref: 'forge', owner: 'Org', repository: 'Project', issue_number: 4 };
        act(() => { expect(result.current.start('Read this issue', 'en-US', [], false, null, [issue])).toBe(true); });
        issue.issue_number = 5;
        await act(async () => { await result.current.retryDelivery(); });
        expect(sendMessage.mock.calls[0][1]).toMatchObject({ selected_issues: [{
            connection_ref: 'forge', owner: 'Org', repository: 'Project', issue_number: 4,
        }] });
        expect(sendMessage.mock.calls[1]).toEqual(sendMessage.mock.calls[0]);
    });

    it('retries a personal request with its original device selection', async () => {
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: null }) }));
        const sendMessage = vi.fn((_type: number, _payload: unknown, _to?: string, request?: string) => request!)
            .mockImplementationOnce(() => { throw new Error('offline'); });
        const subscribe = () => () => undefined;
        const { result, rerender } = renderHook(({ targets }) => useAiAssistantChat({
            deskId: '', conversationStorageScope: 'personal:7', targetRefs: targets, subscribe, sendMessage,
        }), { initialProps: { targets: ['mac', 'linux'] } });
        act(() => { expect(result.current.start('Compare these devices')).toBe(true); });
        rerender({ targets: ['linux'] });
        await act(async () => { await result.current.retryDelivery(); });
        expect(sendMessage.mock.calls[0][1]).toMatchObject({ target_refs: ['mac', 'linux'] });
        expect(sendMessage.mock.calls[1]).toEqual(sendMessage.mock.calls[0]);
    });

    it('samples the sending client timezone per input and freezes it for retries', async () => {
        let timezone = 'Asia/Shanghai';
        const options = Intl.DateTimeFormat().resolvedOptions();
        const clock = vi.spyOn(Intl.DateTimeFormat.prototype, 'resolvedOptions')
            .mockImplementation(() => ({ ...options, timeZone: timezone }));
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: null }) }));
        let offline = true;
        const sendMessage = vi.fn((_type: number, _payload: unknown, _to?: string, request?: string) => {
            if (offline) throw new Error('offline');
            return request!;
        });
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'client-timezone',
            subscribe: () => () => undefined, sendMessage }));
        try {
            act(() => { expect(result.current.start('昨天做了什么？')).toBe(true); });
            expect(sendMessage.mock.calls[0][1]).toMatchObject({ client_timezone: 'Asia/Shanghai' });
            timezone = 'America/Los_Angeles';
            offline = false;
            await act(async () => { await result.current.retryDelivery(); });
            expect(sendMessage.mock.calls[1]).toEqual(sendMessage.mock.calls[0]);
            act(() => result.current.reset());
            act(() => { expect(result.current.start('昨天做了什么？')).toBe(true); });
            expect(sendMessage.mock.calls[2][1]).toMatchObject({ client_timezone: 'America/Los_Angeles' });
        } finally { unmount(); clock.mockRestore(); }
    });

    it('does not stack snapshot polls behind a slow response', async () => {
        vi.useFakeTimers();
        const scope = 'slow-snapshot-poll';
        localStorage.setItem(`ai-assistant-conversation:${scope}`, 'slow-conversation');
        const pending: Array<(value: unknown) => void> = [];
        const fetchMock = vi.fn(() => new Promise(resolve => { pending.push(resolve); }));
        vi.stubGlobal('fetch', fetchMock);
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: scope,
            subscribe: () => () => undefined, sendMessage: vi.fn(() => 'request') }));
        try {
            const snapshotCalls = () => fetchMock.mock.calls.filter(([url]) =>
                String(url).includes('/api/my/ai-assistant-session?')).length;
            await act(async () => { await vi.advanceTimersByTimeAsync(0); });
            const initial = snapshotCalls();
            await act(async () => { await vi.advanceTimersByTimeAsync(2_000); });
            expect(snapshotCalls()).toBe(initial + 1);
            await act(async () => { await vi.advanceTimersByTimeAsync(10_000); });
            expect(snapshotCalls()).toBe(initial + 1);
            expect(result.current.historyRestoreFailed).toBe(false);
        } finally { unmount(); vi.useRealTimers(); localStorage.removeItem(`ai-assistant-conversation:${scope}`); }
    });

    it.each(['saved', 'storage event'])('reports failed %s history restoration and starts a distinct conversation after reset', async (entry) => {
        const scope = `restore-failure-${entry}`;
        const key = `ai-assistant-conversation:${scope}`;
        const previous = 'obsolete-conversation';
        if (entry === 'saved') localStorage.setItem(key, previous);
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({
            success: false, code: deskErrorCodeEnum.SYSTEM_ERROR,
            message: 'decode agent session snapshot: unknown field `model_history_exclusions`',
        }) }));
        const sendMessage = vi.fn(() => 'new-request');
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: scope,
            subscribe: () => () => undefined, sendMessage }));
        try {
            if (entry === 'storage event') act(() => {
                localStorage.setItem(key, previous);
                window.dispatchEvent(new StorageEvent('storage', { key, newValue: previous }));
            });
            await waitFor(() => expect(result.current.error).toBe('history_restore_failed'));
            expect(result.current.hydrating).toBe(false);
            expect(result.current.messages).toEqual([]);
            expect(result.current.historyRestoreFailed).toBe(true);
            act(() => { expect(result.current.start('must not continue the hidden old conversation')).toBe(false); });
            expect(sendMessage).not.toHaveBeenCalled();
            expect(localStorage.getItem(key)).toBe(previous);
            await act(async () => { expect(await result.current.initializeApprovalConversation(true)).toBe(false); });
            expect(result.current.historyRestoreFailed).toBe(true);
            act(() => { expect(result.current.start('still unavailable after another settings failure')).toBe(false); });
            expect(sendMessage).not.toHaveBeenCalled();
            act(() => result.current.reset());
            expect(result.current.error).toBeNull();
            expect(result.current.historyRestoreFailed).toBe(false);
            expect(localStorage.getItem(key)).toBeNull();
            act(() => { expect(result.current.start('new question')).toBe(true); });
            expect(sendMessage).toHaveBeenCalledTimes(1);
            expect(result.current.conversationId).not.toBe(previous);
            expect(result.current.conversationId).toBe(localStorage.getItem(key));
            expect(sendMessage).toHaveBeenCalledWith(SIGNALING_TYPE_CODE_ASK_AI_ASSISTANT,
                expect.objectContaining({ conversation_id: result.current.conversationId, question: 'new question' }),
                scope, expect.any(String));
        } finally { unmount(); }
    });

    it('recovers a failed initial restore by retrying the same conversation', async () => {
        localStorage.setItem('ai-assistant-conversation:restore-retry', 'previous');
        const fetcher = vi.fn().mockRejectedValueOnce(new Error('Connection interrupted')).mockResolvedValue({
            ok: true, json: async () => ({ data: { ...snapshotFields,
                sessionId: 'previous-session', seq: 1, active: false,
                messages: [{ id: 'answer', role: 'assistant', text: 'Earlier answer' }],
            } }),
        });
        vi.stubGlobal('fetch', fetcher);
        const sendMessage = vi.fn(() => 'request');
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'restore-retry',
            subscribe: () => () => undefined, sendMessage }));
        try {
            await waitFor(() => expect(result.current.historyRestoreFailed).toBe(true));
            act(() => result.current.refreshSnapshot());
            await waitFor(() => expect(result.current.messages[0]?.text).toBe('Earlier answer'));
            expect(result.current.historyRestoreFailed).toBe(false);
            expect(result.current.error).toBeNull();
            expect(localStorage.getItem('ai-assistant-conversation:restore-retry')).toBe('previous');
            expect(sendMessage).not.toHaveBeenCalled();
            act(() => { expect(result.current.start('continue')).toBe(true); });
            expect(sendMessage).toHaveBeenCalledWith(SIGNALING_TYPE_CODE_ASK_AI_ASSISTANT,
                expect.objectContaining({ conversation_id: 'previous' }), 'restore-retry', expect.any(String));
        } finally { unmount(); }
    });

    it('does not apply a failed old restore after starting a new conversation', async () => {
        localStorage.setItem('ai-assistant-conversation:restore-late', 'previous');
        let rejectRestore: (reason: Error) => void = () => undefined;
        vi.stubGlobal('fetch', vi.fn(() => new Promise((_resolve, reject) => { rejectRestore = reject; })));
        const sendMessage = vi.fn(() => 'new-request');
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'restore-late',
            subscribe: () => () => undefined, sendMessage }));
        try {
            act(() => result.current.reset());
            act(() => { expect(result.current.start('new question')).toBe(true); });
            await act(async () => rejectRestore(new Error('Old request failed')));
            expect(result.current.error).toBeNull();
            expect(result.current.historyRestoreFailed).toBe(false);
            expect(result.current.messages[0]?.text).toBe('new question');
        } finally { unmount(); }
    });

    it('keeps a newer successful snapshot when the initial restore fails late', async () => {
        localStorage.setItem('ai-assistant-conversation:restore-newer', 'previous');
        let rejectRestore: (reason: Error) => void = () => undefined;
        vi.stubGlobal('fetch', vi.fn().mockImplementationOnce(() => new Promise((_resolve, reject) => { rejectRestore = reject; }))
            .mockResolvedValue({ ok: true, json: async () => ({ data: { ...snapshotFields,
                sessionId: 'previous-session', seq: 2, active: false,
                messages: [{ id: 'answer', role: 'assistant', text: 'Current answer' }],
            } }) }));
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'restore-newer',
            subscribe: () => () => undefined, sendMessage: () => 'request' }));
        try {
            act(() => result.current.refreshSnapshot());
            await waitFor(() => expect(result.current.messages[0]?.text).toBe('Current answer'));
            await act(async () => rejectRestore(new Error('Old request failed')));
            expect(result.current.error).toBeNull();
            expect(result.current.historyRestoreFailed).toBe(false);
            expect(result.current.messages[0]?.text).toBe('Current answer');
        } finally { unmount(); }
    });

    it('sends a large question unchanged and leaves the draft unacknowledged after server refusal', async () => {
        const text = '正文🙂'.repeat(10000);
        const failure = 'AI Assistant question must be 1..=16384 bytes';
        let subscriber: SignalingSubscriber | null = null;
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: {
            ...snapshotFields, sessionId: 'large-session', seq: 10, active: false,
            messages: [],
        } }) }));
        const sendMessage = vi.fn((type: number, _payload?: unknown) => type === SIGNALING_TYPE_CODE_SELECT_AI_ASSISTANT_SESSION
            ? 'session-request' : 'turn-request');
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'large-question', connected: true,
            subscribe: handler => { subscriber = handler; return () => undefined; }, sendMessage }));
        try {
            act(() => subscriber?.({ request_id: 'session-request', signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_SESSION_SELECTED,
                signaling_data: { revision: 1, target: { target_id: 'desktop', display_name: 'Desktop', foreground: true,
                    remote_desktop_ready: true, terminal_ready: true, file_ready: true, assistant_ready: true } },
                response_state: { error_code: 0, message: '' } }));
            act(() => { expect(result.current.start(text)).toBe(true); });
            const sent = sendMessage.mock.calls.find(([type]) => type === SIGNALING_TYPE_CODE_ASK_AI_ASSISTANT);
            expect(sent?.[1]).toMatchObject({ question: text });
            await act(async () => subscriber?.({ request_id: 'turn-request', signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED,
                signaling_data: { request_id: 'turn-request', seq: 1, kind: 'error', error: { message: failure } } }));
            expect(result.current.error).toBe(failure);
            expect(result.current.acceptedInput).toBeNull();
            expect(result.current.deliveryState).toBeNull();
        } finally { unmount(); }
    });

    it('restores native bound citations from the owner snapshot without sending another turn', async () => {
        localStorage.setItem('ai-assistant-conversation:citations-native', 'conversation');
        const citation = {answer_message_id:'answer',answer_revision:1,source_id:'src_AAECAwQFBgcICQoLDA0ODw',source_revision:2};
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ok:true,json:async()=>({success:true,data:{
            ...snapshotFields,sessionId:'session',seq:1,active:false,messages:[{id:'answer',role:'assistant',text:`Answer 〔来源:${citation.source_id}〕`,
                citations:[citation],citationsIncomplete:true}],
        }})}));
        const sendMessage = vi.fn(() => 'request');
        const {result,unmount} = renderHook(()=>useAiAssistantChat({deskId:'citations-native',subscribe:()=>()=>undefined,sendMessage}));
        try {
            await waitFor(()=>expect(result.current.messages.find(message=>message.id==='answer')?.citations).toEqual([citation]));
            expect(result.current.messages.find(message=>message.id==='answer')?.citationsIncomplete).toBe(true);
            expect(sendMessage).not.toHaveBeenCalled();
        } finally { unmount(); }
    });
    it('restores temporary mode from the owner snapshot and clears it for an ordinary new conversation', async () => {
        localStorage.setItem('ai-assistant-conversation:temporary-restore', 'conversation');
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: {
            ...snapshotFields, sessionId: 'session', seq: 1, active: false, messages: [], temporaryConversation: true,
        } }) }));
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'temporary-restore',
            subscribe: () => () => undefined, sendMessage: () => 'request' }));
        try {
            await waitFor(() => expect(result.current.temporaryConversation).toBe(true));
            act(() => { result.current.reset(); });
            expect(result.current.temporaryConversation).toBe(false);
        } finally { unmount(); }
    });
    it('enables approval when configuration becomes ready without a new conversation event', async () => {
        vi.useFakeTimers();
        localStorage.setItem('ai-assistant-conversation:desk-1', 'conversation-1');
        let available = false;
        let opened = false;
        const delegation = { delegationId: 'delegation', status: 'active', reviewsUsed: 0, tokensUsed: 0 };
        const fetchMock = vi.fn(async (url: string, _options?: RequestInit) => {
            if (url.endsWith('/approval-delegation/open')) {
                opened = true;
                return { ok: true, json: async () => ({ success: true, data: delegation }) };
            }
            return { ok: true, json: async () => ({ success: true, data: {
                ...snapshotFields, sessionId: 'session', seq: 1, active: false, messages: [],
                approvalModelReadiness: { available, reason: available ? null : 'approval_model_disabled' },
                approvalDelegation: opened ? delegation : null,
            } }) };
        });
        vi.stubGlobal('fetch', fetchMock);
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'desk-1',
            subscribe: () => () => undefined, sendMessage: () => 'request' }));
        try {
            await act(async () => { await Promise.resolve(); });
            expect(result.current.approvalModelReadiness?.available).toBe(false);
            await act(async () => { expect(await result.current.setAutomaticApproval(true)).toBe(false); });
            expect(opened).toBe(false);
            available = true;
            await act(async () => { await vi.advanceTimersByTimeAsync(2_000); });
            expect(result.current.approvalModelReadiness?.available).toBe(true);
            await act(async () => { expect(await result.current.setAutomaticApproval(true)).toBe(true); });
            expect(result.current.approvalDelegation?.status).toBe('active');
            expect(fetchMock).toHaveBeenCalledWith('/api/my/ai-assistant-session/approval-delegation/open',
                expect.objectContaining({ method: 'POST', body: expect.any(String) }));
            const call = fetchMock.mock.calls.find(([url]) => url.endsWith('/approval-delegation/open'));
            expect(JSON.parse(call?.[1]?.body as string)).toMatchObject({
                conversation: 'conversation-1', session: 'session', expectedInputRevision: 1,
            });
        } finally { unmount(); }
    });
    it.each([
        ['paused', false, false], ['blocked', false, false], ['waiting_user', false, false],
        ['queued', false, true], ['waiting_work', false, true], ['waiting_approval', false, true],
        ['waiting_device', false, true], ['waiting_model', false, true], ['running', false, true],
        ['paused', true, true],
    ])('shows the stop control for %s with background work=%s: %s', async (state, background, expected) => {
        localStorage.setItem('ai-assistant-conversation:goal-stop', 'conversation');
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: {
            ...snapshotFields, sessionId: 'session', seq: 1, active: false, messages: [],
            goal: { goalId: 'goal', state, stateVersion: 1, goalText: 'Continue' },
            commandTasks: background ? [{ taskId: 'command', callId: 'call', executionGeneration: 'generation', state: 'running', updatedAt: 'now' }] : [],
        } }) }));
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'goal-stop',
            subscribe: () => () => undefined, sendMessage: () => 'request' }));
        try {
            await waitFor(() => expect(result.current.goal?.state).toBe(state));
            expect(result.current.canStop).toBe(expected);
        } finally { unmount(); }
    });
    it('clears a disconnected local turn when a newer idle snapshot settles it', async () => {
        vi.useFakeTimers();
        localStorage.setItem('ai-assistant-conversation:recovered-stop', 'conversation');
        let subscriber: SignalingSubscriber | null = null;
        let seq = 1;
        vi.stubGlobal('fetch', vi.fn().mockImplementation(async () => ({ ok: true, json: async () => ({ data: {
            ...snapshotFields, sessionId: 'session', seq: seq++, active: false, messages: [],
            goal: { goalId: 'goal', state: 'paused', stateVersion: 1, goalText: 'Continue' },
        } }) })));
        const sendMessage = vi.fn((_type: number, _data: unknown, _to?: string, id?: string) => id!);
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'recovered-stop',
            subscribe: handler => { subscriber = handler; return () => undefined; }, sendMessage }));
        try {
            await act(async () => { await Promise.resolve(); });
            act(() => { result.current.start('Continue after recovery'); });
            const requestId = sendMessage.mock.calls.at(-1)![3]!;
            act(() => subscriber?.({ request_id: requestId, signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED,
                signaling_data: { request_id: requestId, seq: 1, kind: 'status', status: 'accepted' } }));
            expect(result.current.canStop).toBe(true);
            await act(async () => { await vi.advanceTimersByTimeAsync(2_000); });
            expect(result.current.turnRunning).toBe(false);
            expect(result.current.canStop).toBe(false);
        } finally { unmount(); }
    });
    it('does not let a read started before goal control overwrite its committed state', async () => {
        vi.useFakeTimers();
        localStorage.setItem('ai-assistant-conversation:desk-1', 'conversation-1');
        const pending: Array<(value: unknown) => void> = [];
        const goal = { goalId: 'goal', state: 'running', stateVersion: 1, goalText: 'Calculate 6+8' };
        stubSnapshotFetch(vi.fn((url: string) => url.endsWith('/goal/control')
            ? Promise.resolve({ ok: true, json: async () => ({ success: true, data: { ...goal, state: 'paused', stateVersion: 2 } }) })
            : new Promise(resolve => pending.push(resolve))));
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'desk-1', subscribe: () => () => undefined, sendMessage: () => 'request' }));
        const response = (seq: number, value: typeof goal) => ({ ok: true, json: async () => ({ data: {
            ...snapshotFields, sessionId: 'session', seq, active: false, messages: [], goal: value,
        } }) });
        await act(async () => { pending[0](response(1, goal)); });
        await act(async () => { await vi.advanceTimersByTimeAsync(2_000); });
        let action: Promise<boolean>;
        await act(async () => { action = result.current.controlGoal('pause'); await Promise.resolve(); });
        expect(result.current.goal?.state).toBe('paused');
        expect(pending).toHaveLength(3);
        await act(async () => { pending[1](response(2, goal)); });
        expect(result.current.goal?.state).toBe('paused');
        await act(async () => { pending[2](response(3, { ...goal, state: 'paused', stateVersion: 2 })); await action!; });
        expect(result.current.goal?.stateVersion).toBe(2);
        unmount();
    });
    it('sends explicit goal intent with the completed goal being continued', async () => {
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: null }) }));
        const sendMessage = vi.fn((_type: number, _data: unknown, _to?: string, id?: string) => id!);
        const { result, unmount } = renderHook(() => useAiAssistantChat({
            deskId: 'goal-intent', subscribe: () => () => undefined, sendMessage,
        }));
        act(() => { expect(result.current.start('Finish the report', 'zh', [], true, 'completed-goal')).toBe(true); });
        expect(sendMessage.mock.calls.at(-1)?.[1]).toMatchObject({
            start_goal: true,
            previous_completed_goal_id: 'completed-goal',
        });
        unmount();
    });
    it('ignores host turn frames even with the pending controller request id', async () => {
        let subscriber: SignalingSubscriber | null = null;
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: null }) }));
        const sendMessage = vi.fn((_type: number, _data: unknown, _to?: string, id?: string) => id!);
        const { result, unmount } = renderHook(() => useAiAssistantChat({
            deskId: 'host-turn-role',
            subscribe: handler => { subscriber = handler; return () => undefined; },
            sendMessage,
        }));
        try {
            act(() => { result.current.start('Pending controller input'); });
            const request_id = sendMessage.mock.calls[0][3]!;
            for (const signaling_type of [SIGNALING_TYPE_CODE_QUERY_COMPUTER_ACTION_TURN, SIGNALING_TYPE_CODE_COMPUTER_ACTION_TURN_STATUS]) {
                act(() => subscriber?.({ request_id, signaling_type,
                    signaling_data: { seq: 1, kind: 'status', status: 'accepted' } }));
                expect(result.current.deliveryState).toBe('sending');
                expect(result.current.acceptedInput).toBeNull();
            }
            act(() => subscriber?.({ request_id, signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED,
                signaling_data: { request_id, seq: 1, kind: 'status', status: 'accepted' } }));
            expect(result.current.deliveryState).toBeNull();
            expect(result.current.acceptedInput?.question).toBe('Pending controller input');
        } finally { unmount(); }
    });

    it('does not send live desktop metadata as an explicit object attachment', async () => {
        localStorage.setItem('ai-assistant-conversation:delivery-kinds', 'conversation');
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: {
            ...snapshotFields,
            sessionId: 'session', seq: 1, active: false, messages: [],
            contextAttachments: [{ id: 'desktop-context', kind: 'interactive_session', capabilityId: 'desktop.ui.inspect',
                providerId: 'desktop.ui', state: 'active', expiresAtUnixMs: 1, createdAtUnixMs: 0, displaySummary: 'desktop' }],
        } }) }));
        const sendMessage = vi.fn((_type: number, _data: unknown, _to?: string, id?: string) => id!);
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'delivery-kinds', subscribe: () => () => undefined, sendMessage }));
        await waitFor(() => expect(result.current.attachments).toHaveLength(1));
        act(() => { expect(result.current.start('Continue', 'zh', ['desktop.ui.inspect'])).toBe(true); });
        const payload = sendMessage.mock.calls.at(-1)?.[1] as Record<string, unknown>;
        expect(payload.selected_attachment_ids).toEqual([]);
        expect(payload.selected_capability_ids).toEqual(['desktop.ui.inspect']);
        unmount();
    });
    it('preserves intake rejection across an old successful snapshot and permits a new send', async () => {
        let subscriber: SignalingSubscriber | null = null;
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: {
            ...snapshotFields,
            sessionId: 'session', seq: 10, active: false, messages: [{ id: 'old-answer', role: 'assistant', text: 'Previous success' }],
        } }) }));
        const sendMessage = vi.fn((_type: number, _data: unknown, _to?: string, id?: string) => id!);
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'delivery-reject',
            subscribe: handler => { subscriber = handler; return () => undefined; }, sendMessage }));
        act(() => { result.current.start('New requirement'); });
        const original = sendMessage.mock.calls.at(-1)!;
        await act(async () => subscriber?.({ request_id: original[3], signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED,
            signaling_data: { request_id: original[3], seq: 1, kind: 'error', error: { message: 'selected attachment is unavailable' } } }));
        expect(result.current.error).toBe('selected attachment is unavailable');
        expect(result.current.deliveryState).toBeNull();
        expect(result.current.acceptedInput).toBeNull();
        await act(async () => { await result.current.retryDelivery(); });
        expect(sendMessage).toHaveBeenCalledTimes(1);
        act(() => { expect(result.current.start('Corrected requirement')).toBe(true); });
        expect(sendMessage.mock.calls.at(-1)?.[3]).not.toBe(original[3]);
        unmount();
    });
    it('bounds the snapshot lookup before retrying an unconfirmed message', async () => {
        vi.useFakeTimers();
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: null }) }));
        const sendMessage = vi.fn((_type: number, _data: unknown, _to?: string, id?: string): string => { throw new Error('offline'); });
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'delivery-query-timeout', subscribe: () => () => undefined, sendMessage }));
        try {
            act(() => { result.current.start('Retry after query timeout'); });
            vi.stubGlobal('fetch', vi.fn((_url: string, init: RequestInit) => new Promise((_resolve, reject) => {
                init.signal?.addEventListener('abort', () => reject(new Error('aborted')));
            })));
            await act(async () => {
                const retry = result.current.retryDelivery();
                await vi.advanceTimersByTimeAsync(30_000);
                await retry;
            });
            expect(sendMessage).toHaveBeenCalledTimes(2);
            expect(sendMessage.mock.calls[1]).toEqual(sendMessage.mock.calls[0]);
        } finally { unmount(); vi.useRealTimers(); }
    });
    it('shows a retry state when no server receipt arrives and ignores unrelated receipts', async () => {
        vi.useFakeTimers();
        let subscriber: SignalingSubscriber | null = null;
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: null }) }));
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'delivery-timeout',
            subscribe: handler => { subscriber = handler; return () => undefined; },
            sendMessage: (_type, _data, _to, id) => id!,
        }));
        try {
            act(() => { result.current.start('Retain on timeout'); });
            act(() => subscriber?.({ request_id: 'another-request', signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED,
                signaling_data: { seq: 1, kind: 'status', status: 'accepted' } }));
            expect(result.current.deliveryState).toBe('sending');
            await act(async () => { await vi.advanceTimersByTimeAsync(15_000); });
            expect(result.current.deliveryState).toBe('unconfirmed');
            expect(result.current.acceptedInput).toBeNull();
            expect(result.current.messages.at(-1)?.text).toBe('Retain on timeout');
        } finally { unmount(); vi.useRealTimers(); }
    });
    it('retains unconfirmed input and retries the exact payload and message ID', async () => {
        let subscriber: SignalingSubscriber | null = null;
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: null }) }));
        const sendMessage = vi.fn((_type: number, _data: unknown, _to?: string, id?: string): string => {
            if (sendMessage.mock.calls.length === 1) throw new Error('Disconnected');
            return id!;
        });
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'delivery-test',
            subscribe: handler => { subscriber = handler; return () => undefined; }, sendMessage }));
        act(() => { result.current.reset(true); });
        expect(result.current.temporaryConversation).toBe(true);
        act(() => { expect(result.current.start('Keep this message')).toBe(true); });
        expect(result.current.deliveryState).toBe('unconfirmed');
        expect(result.current.messages.at(-1)?.text).toBe('Keep this message');
        expect(result.current.acceptedInput).toBeNull();
        const original = sendMessage.mock.calls[0];
        expect(original[1]).toMatchObject({ temporary_conversation: true });
        await act(async () => { await result.current.retryDelivery(); });
        expect(sendMessage.mock.calls[1]).toEqual(original);
        expect(result.current.messages.filter(m => m.role === 'user')).toHaveLength(1);
        act(() => subscriber?.({ request_id: original[3], signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED,
            signaling_data: { request_id: original[3], seq: 1, kind: 'status', status: 'accepted' } }));
        expect(result.current.deliveryState).toBeNull();
        expect(result.current.acceptedInput?.question).toBe('Keep this message');
        unmount();
    });
    it('queries the stored message before retrying and does not resend an accepted input', async () => {
        const sendMessage = vi.fn((_type: number, _data: unknown, _to?: string, _id?: string): string => { throw new Error('Receipt lost'); });
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: null }) }));
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'delivery-stored',
            subscribe: () => () => undefined, sendMessage }));
        act(() => { result.current.start('Already stored'); });
        const payload = sendMessage.mock.calls[0][1] as { client_message_id: string };
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: {
            ...snapshotFields,
            sessionId: 'stored-session', seq: 1, active: false,
            messages: [{ id: payload.client_message_id, role: 'user', text: 'Already stored' }],
        } }) }));
        await act(async () => { await result.current.retryDelivery(); });
        expect(sendMessage).toHaveBeenCalledTimes(1);
        expect(result.current.deliveryState).toBeNull();
        expect(result.current.acceptedInput?.id).toBe(payload.client_message_id);
        unmount();
    });
    it('accepts directory acknowledgements only for the exact response type and conversation request', async () => {
        let subscriber: SignalingSubscriber | null = null;
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: null }) }));
        const sendMessage = vi.fn((_type: number, _data?: unknown, _to?: string) => 'directory-wire');
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'directory-ack', connected: true,
            subscribe: handler => { subscriber = handler; return () => undefined; }, sendMessage }));
        await waitFor(() => expect(result.current.hydrating).toBe(false));
        act(() => { expect(result.current.updateDirectory({ kind: 'select_directory', path: '/private/tmp/test', purpose: 'test', expected_revision: 0 }, 'timeout')).toBe(true); });
        const request = sendMessage.mock.calls.at(-1)?.[1] as { conversation_id: string; client_request_id: string };
        const ack = { request_id: 'directory-wire', signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_OBJECT_CONTEXT_UPDATED,
            signaling_data: { conversation_id: request.conversation_id, client_request_id: request.client_request_id, changed: true } };
        act(() => subscriber?.({ ...ack, signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_CONTEXT_UPDATED }));
        expect(result.current.contextUpdating).toBe(true);
        act(() => subscriber?.({ ...ack, signaling_data: { ...ack.signaling_data, conversation_id: 'another-conversation' } }));
        expect(result.current.contextUpdating).toBe(true);
        act(() => subscriber?.({ ...ack, signaling_data: { ...ack.signaling_data, client_request_id: 'another-request' } }));
        expect(result.current.contextUpdating).toBe(true);
        await act(async () => subscriber?.(ack));
        expect(result.current.contextUpdating).toBe(false);
        unmount();
    });
    it('restores a pending directory proposal as permission required, not a missing-answer error', async () => {
        localStorage.setItem('ai-assistant-conversation:pending-directory', 'saved-conversation');
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: {
            ...snapshotFields,
            sessionId: 'saved-conversation', seq: 10, active: false,
            fileScope: { revision: 1, directories: [{ requestId: 'directory-request', canonicalPath: '/private/tmp/test',
                purpose: 'report', state: 'pending', source: 'model_proposal', referenceExpiresAt: '2030-01-01T00:00:00Z' }] },
            messages: [{ id: 'user-1', role: 'user', text: 'create a report' }],
        } }) }));
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'pending-directory', connected: true,
            subscribe: () => () => undefined, sendMessage: () => 'select' }));
        await waitFor(() => expect(result.current.status).toBe('permission_required'));
        expect(result.current.error).toBeNull();
        expect(result.current.fileScope.revision).toBe(1);
        unmount();
    });
    it('routes a selectable directory decision to the device recorded in the snapshot', async () => {
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: {
            ...snapshotFields, sessionId: 'account-session', seq: 10, active: false, messages: [],
            fileScope: { revision: 2, directories: [{ requestId: 'original-directory', deviceRef: 'device-a',
                canonicalPath: '/work', purpose: 'report', state: 'approved', source: 'owner_selection', referenceExpiresAt: '' }] },
        } }) }));
        const sendMessage = vi.fn((_type: number, _payload?: unknown, _to?: string) => 'directory-wire');
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: '', conversationStorageScope: 'personal:directory',
            initialConversationId: 'account-conversation', connected: true, subscribe: () => () => undefined, sendMessage }));
        await waitFor(() => expect(result.current.fileScope.revision).toBe(2));
        const revoke = { kind: 'revoke_directory' as const, directory_request_id: 'original-directory', expected_revision: 2 };
        act(() => { expect(result.current.updateDirectory(revoke, 'timeout', 'device-b')).toBe(true); });
        expect(sendMessage).toHaveBeenCalledWith(SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_OBJECT_CONTEXT,
            expect.objectContaining({ conversation_target: 'selectable_devices', operation: revoke }), 'device-a');
        unmount();
    });
    it('selects live resources on the requested device and freezes only admitted attachments', async () => {
        const liveAttachment = (deviceRef: string) => ({
            id: `live-${deviceRef}`, deviceRef, kind: 'interactive_session', providerId: 'office.document',
            capabilityId: 'office.document.inspect', displaySummary: 'live document', state: 'active',
            createdAtUnixMs: 1, expiresAtUnixMs: Date.now() + 60_000,
        });
        stubSnapshotFetch(async () => ({ ok: true, json: async () => ({ data: {
            ...snapshotFields, sessionId: 'account-session', seq: 10, active: false, messages: [],
            contextAttachments: ['device-a', 'device-b'].map(liveAttachment),
        } }) }));
        const sendMessage = vi.fn((_type: number, _payload?: unknown, _to?: string, id?: string) => id ?? 'context-wire');
        let subscriber: SignalingSubscriber | null = null;
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: '', conversationStorageScope: 'personal:live',
            initialConversationId: 'account-conversation', targetRefs: ['device-b'], connected: true,
            subscribe: handler => { subscriber = handler; return () => undefined; }, sendMessage }));
        await waitFor(() => expect(result.current.attachments).toHaveLength(2));
        act(() => { expect(result.current.updateContext(['office.document.inspect'], 'device-b')).toBe(true); });
        const payload = sendMessage.mock.calls.find(call => call[0] === SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_CONTEXT)?.[1] as
            { conversation_id: string; client_request_id: string };
        expect(sendMessage).toHaveBeenCalledWith(SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_CONTEXT,
            expect.objectContaining({ conversation_target: 'selectable_devices', selected_capability_ids: ['office.document.inspect'] }), 'device-b');
        act(() => subscriber?.({ signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_CONTEXT_UPDATED, request_id: 'context-wire',
            signaling_data: { ...payload, changed: true } }));
        act(() => { expect(result.current.start('Read the open document')).toBe(true); });
        expect(sendMessage).toHaveBeenCalledWith(SIGNALING_TYPE_CODE_ASK_AI_ASSISTANT,
            expect.objectContaining({ target_refs: ['device-b'], selected_capability_ids: [], selected_attachment_ids: ['live-device-b'] }),
            undefined, expect.any(String));
        unmount();
    });
    it('pages each selectable preview on its original device even when preview IDs match', async () => {
        let subscriber: SignalingSubscriber | null = null;
        stubSnapshotFetch(async () => ({ ok: true, json: async () => ({ data: {
            ...snapshotFields, sessionId: 'account-session', seq: 10, active: false, messages: [],
        } }) }));
        const sendMessage = vi.fn((_type: number, _payload?: unknown, _to?: string, id?: string) => id ?? 'page-wire');
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: '', conversationStorageScope: 'personal:preview',
            initialConversationId: 'account-conversation', targetRefs: ['device-a', 'device-b'], connected: true,
            subscribe: handler => { subscriber = handler; return () => undefined; }, sendMessage }));
        await waitFor(() => expect(result.current.sessionId).toBe('account-session'));
        act(() => { expect(result.current.start('Preview the two documents')).toBe(true); });
        const requestId = sendMessage.mock.calls.find(([type]) => type === SIGNALING_TYPE_CODE_ASK_AI_ASSISTANT)?.[3];
        const frame = { descriptor: { preview_id: 'preview', page_count: 3 }, page: { page: 1 }, preview_data_url: 'data:image/png;base64,AAAA' };
        act(() => {
            ['device-a', 'device-b'].forEach((device, seq) => subscriber?.({ request_id: requestId,
                signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED, signaling_data: {
                    request_id: requestId, seq: seq + 1, kind: 'document_preview', document_preview: frame, document_preview_target_ref: device,
                } }));
        });
        expect(result.current.documentPreviews.map(preview => preview.deviceRef)).toEqual(['device-a', 'device-b']);
        act(() => { expect(result.current.requestDocumentPreviewPage('preview', 2, 'device-b')).toBe(true); });
        expect(sendMessage).toHaveBeenLastCalledWith(SIGNALING_TYPE_CODE_REQUEST_DOCUMENT_PREVIEW_PAGE,
            { conversation_id: 'account-session', conversation_target: 'selectable_devices', preview_id: 'preview', page: 2, spec_version: 1 }, 'device-b');
        act(() => subscriber?.({ request_id: 'page-wire', signaling_type: SIGNALING_TYPE_CODE_DOCUMENT_PREVIEW_PAGE_UPDATED,
            signaling_data: { conversation_id: 'account-session', preview_id: 'preview', page: 2,
                frame: { page: { page: 2 }, preview_data_url: 'data:image/png;base64,BBBB' } },
        }));
        expect(result.current.documentPreviews.map(preview => preview.page.page)).toEqual([1, 2]);
        unmount();
    });
    it.each([
        [undefined, 'model context compression failed: stale_context'],
        [deskErrorCodeEnum.SCHEDULE_MODEL_BUDGET_EXCEEDED,
            'The scheduled task has insufficient model-token budget remaining. This model request was not sent. Review the task budget before running it again.'],
    ])('restores and localizes durable terminal error code %s in a newly opened page', async (errorCode, expected) => {
        localStorage.setItem('ai-assistant-conversation:restore-error', 'saved-conversation');
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: {
            ...snapshotFields,
            sessionId: 'saved-conversation', seq: 10, active: false,
            terminalError: { message: 'model context compression failed: stale_context', error_code: errorCode },
            messages: [{ id: 'user-1', role: 'user', text: 'continue' }],
        } }) }));
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'restore-error', connected: true,
            subscribe: () => () => undefined, sendMessage: () => 'select' }));
        await waitFor(() => expect(result.current.error).toBe(expected));
        expect(result.current.status).toBe('error');
        unmount();
    });
    it.each([
        [undefined, 'Context authorization expired.'],
        [deskErrorCodeEnum.SCHEDULE_MODEL_BUDGET_EXCEEDED,
            'The scheduled task has insufficient model-token budget remaining. This model request was not sent. Review the task budget before running it again.'],
    ])('keeps localized turn error code %s after refresh and clears it on a new turn', async (errorCode, expected) => {
        let subscriber: SignalingSubscriber | null = null;
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: {
            ...snapshotFields,
            sessionId: 'failed-session', seq: 10, active: false,
            messages: [{ id: 'user-1', role: 'user', text: 'continue' }],
        } }) }));
        const sendMessage = vi.fn((type: number) => type === SIGNALING_TYPE_CODE_SELECT_AI_ASSISTANT_SESSION
            ? 'session-request' : 'turn-request');
        const { result } = renderHook(() => useAiAssistantChat({ deskId: 'failed-desk', connected: true,
            subscribe: (handler) => { subscriber = handler; return () => undefined; }, sendMessage }));
        act(() => subscriber?.({ request_id: 'session-request',
            signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_SESSION_SELECTED,
            signaling_data: { revision: 1, target: { target_id: 'desktop', display_name: 'Desktop',
                foreground: true, remote_desktop_ready: true, terminal_ready: true, file_ready: true, assistant_ready: true } },
            response_state: { error_code: 0, message: '' },
        }));
        act(() => { expect(result.current.start('continue')).toBe(true); });
        await act(async () => subscriber?.({ request_id: 'turn-request',
            signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED,
            signaling_data: { request_id: 'turn-request', seq: 1, kind: 'error', error: { message: 'Context authorization expired.', error_code: errorCode } },
        }));
        await waitFor(() => expect(result.current.messages.at(-1)?.text).toBe('continue'));
        expect(result.current.deliveryState).toBeNull();
        expect(result.current.error).toBe(expected);
        expect(result.current.status).toBe('error');
        act(() => { expect(result.current.start('another question')).toBe(true); });
        expect(result.current.error).toBeNull();
        act(() => result.current.reset());
        expect(result.current.error).toBeNull();
    });


    it('opens history through its continuation id without sending a cancel or a question', async () => {
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: {
            ...snapshotFields,
            sessionId: 'old-session', seq: 1, active: false, messages: [],
        } }) }));
        const sendMessage = vi.fn();
        const { result } = renderHook(() => useAiAssistantChat({ deskId: 'desk-history',
            subscribe: () => () => undefined, sendMessage }));
        act(() => { expect(result.current.selectConversation('old-conversation')).toBe(true); });
        await waitFor(() => expect(result.current.hydrating).toBe(false));
        expect(fetch).toHaveBeenCalledWith(expect.stringContaining('conversation=old-conversation'), expect.anything());
        expect(localStorage.getItem('ai-assistant-conversation:desk-history')).toBe('old-conversation');
        expect(sendMessage).not.toHaveBeenCalled();
    });

    it('opens an attention deep link ahead of the last locally selected conversation', async () => {
        localStorage.setItem('ai-assistant-conversation:attention-device', 'previous-conversation');
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: {
            ...snapshotFields,
            sessionId: 'attention-session', seq: 1, active: false, messages: [],
        } }) }));
        const sendMessage = vi.fn();
        const { result, unmount } = renderHook(() => useAiAssistantChat({
            deskId: 'current-connection', conversationStorageScope: 'attention-device',
            initialConversationId: 'requested-conversation',
            subscribe: () => () => undefined, sendMessage,
        }));
        await waitFor(() => expect(result.current.hydrating).toBe(false));
        expect(fetch).toHaveBeenCalledWith(
            expect.stringContaining('conversation=requested-conversation'), expect.anything(),
        );
        expect(sendMessage).not.toHaveBeenCalled();
        unmount();
    });

    it('switches and creates conversations while a previous turn keeps running', async () => {
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: {
            ...snapshotFields,
            sessionId: 'other', requestId: 'remote-turn', seq: 1, active: true, messages: [],
        } }) }));
        const sendMessage = vi.fn().mockReturnValue('original-turn');
        const { result } = renderHook(() => useAiAssistantChat({ deskId: 'parallel', subscribe: () => () => undefined, sendMessage }));
        act(() => { result.current.start('original question'); });
        act(() => { expect(result.current.selectConversation('other')).toBe(true); });
        await waitFor(() => expect(result.current.hydrating).toBe(false));
        expect(result.current.turnRunning).toBe(true);
        expect(sendMessage).toHaveBeenCalledTimes(1);
        act(() => { result.current.reset(); });
        expect(result.current.turnRunning).toBe(false);
        expect(sendMessage).toHaveBeenCalledTimes(1);
        act(() => { result.current.start('new question'); });
        expect(sendMessage).toHaveBeenCalledTimes(2);
        act(() => { result.current.forgetConversation('other'); });
        expect(result.current.messages[0].text).toBe('new question');
    });

    beforeEach(() => localStorage.clear());
    afterEach(() => {
        vi.useRealTimers();
        vi.unstubAllGlobals();
    });

    it('selects the only AI Assistant desktop session before enabling turns', async () => {
        let subscriber: SignalingSubscriber | null = null;
        const sendMessage = vi.fn((type: number) => type ===
            SIGNALING_TYPE_CODE_SELECT_AI_ASSISTANT_SESSION
            ? 'session-request-1'
            : 'assistant-request-1');
        const { result } = renderHook(() => useAiAssistantChat({
            deskId: 'desk-1',
            connected: true,
            subscribe: (handler) => {
                subscriber = handler;
                return () => { subscriber = null; };
            },
            sendMessage,
        }));

        await waitFor(() => expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_SELECT_AI_ASSISTANT_SESSION,
            {},
            'desk-1',
        ));
        expect(result.current.sessionTargetReady).toBe(false);
        expect(result.current.start('too early')).toBe(false);

        act(() => {
            subscriber?.({
                request_id: 'session-request-1',
                signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_SESSION_SELECTED,
                signaling_data: {
                    revision: 3,
                    target: {
                        target_id: 'target-a',
                        display_name: 'Desktop A',
                        session_type: 'wayland',
                        seat: 'seat0',
                        foreground: true,
                        remote_desktop_ready: true,
                        terminal_ready: true,
                        file_ready: true,
                        assistant_ready: true,
                    },
                },
                response_state: { error_code: 0, message: '' },
            });
        });

        expect(result.current.sessionTargetReady).toBe(true);
        expect(result.current.sessionTarget?.target_id).toBe('target-a');
        expect(result.current.start('continue')).toBe(true);
    });

    it('requires an explicit AI Assistant target when the host returns many', async () => {
        let subscriber: SignalingSubscriber | null = null;
        const sendMessage = vi.fn((type: number) => type ===
            SIGNALING_TYPE_CODE_SELECT_AI_ASSISTANT_SESSION
            ? 'session-request-1'
            : 'assistant-request-1');
        const { result } = renderHook(() => useAiAssistantChat({
            deskId: 'desk-1',
            connected: true,
            subscribe: (handler) => {
                subscriber = handler;
                return () => { subscriber = null; };
            },
            sendMessage,
        }));
        await waitFor(() => expect(sendMessage).toHaveBeenCalledTimes(1));

        act(() => {
            subscriber?.({
                request_id: 'session-request-1',
                signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_SESSION_SELECTED,
                signaling_data: {
                    revision: 2,
                    targets: [{
                        target_id: 'target-a',
                        display_name: 'Desktop A',
                        foreground: true,
                        remote_desktop_ready: true,
                        terminal_ready: true,
                        file_ready: true,
                        assistant_ready: true,
                    }, {
                        target_id: 'target-b',
                        display_name: 'Desktop B',
                        foreground: false,
                        remote_desktop_ready: true,
                        terminal_ready: true,
                        file_ready: true,
                        assistant_ready: true,
                    }],
                },
                response_state: { error_code: 109, message: 'selection required' },
            });
        });

        expect(result.current.sessionTargetReady).toBe(false);
        expect(result.current.sessionTargets.map((target) => target.target_id)).toEqual([
            'target-a',
            'target-b',
        ]);
        act(() => {
            expect(result.current.selectSessionTarget('target-b')).toBe(true);
        });
        expect(sendMessage).toHaveBeenLastCalledWith(
            SIGNALING_TYPE_CODE_SELECT_AI_ASSISTANT_SESSION,
            { session_target_id: 'target-b' },
            'desk-1',
        );
    });

    it('restores browser-safe attachment metadata with the durable conversation', async () => {
        localStorage.setItem('ai-assistant-conversation:desk-1', 'conversation-1');
        vi.stubGlobal('fetch', vi.fn(async () => ({
            ok: true,
            json: async () => ({
                data: {
                    ...snapshotFields,
                    sessionId: 'session-1',
                    seq: 1,
                    messages: [],
                    contextAttachments: [{
                        id: 'attachment-1',
                        kind: 'interactive_session',
                        providerId: 'desktop.ui',
                        capabilityId: 'desktop.ui.inspect',
                        displaySummary: 'current interactive session',
                        createdAtUnixMs: 100,
                        expiresAtUnixMs: 200,
                        state: 'stale',
                        staleReason: 'worker_respawned',
                    }],
                },
            }),
        })));
        const { result } = renderHook(() => useAiAssistantChat({
            deskId: 'desk-1',
            subscribe: () => () => undefined,
            sendMessage: () => 'request',
        }));

        await waitFor(() => expect(result.current.hydrating).toBe(false));
        expect(result.current.attachments).toEqual([
            expect.objectContaining({
                capabilityId: 'desktop.ui.inspect',
                state: 'stale',
                staleReason: 'worker_respawned',
            }),
        ]);
    });

    it.each(['execution failed: command timed out after 240000 ms', 'exit_code=0\n12G /example'])('shows a durable command receipt without waiting for an AI answer: %s', async (output) => {
        localStorage.setItem('ai-assistant-conversation:desk-1', 'conversation-1');
        vi.stubGlobal('fetch', vi.fn(async () => ({
            ok: true,
            json: async () => ({ data: {
                ...snapshotFields,
                sessionId: 'session-1', seq: 34, active: false,
                messages: [
                    { id: 'call', role: 'assistant', text: '', toolCalls: [{ id: 'command-1', name: 'exec_command', argumentsJson: '{}' }] },
                    { id: 'running', role: 'tool', toolCallId: 'command-1', text: '{"status":"background_running"}' },
                    { id: 'waiting', role: 'assistant', text: 'Waiting for the background command.' },
                    { id: 'finished', role: 'untrusted_output', toolCallId: 'command-1', backgroundTaskId: 'exec-1', text: output },
                ],
            } }),
        })));
        const { result } = renderHook(() => useAiAssistantChat({
            deskId: 'desk-1', subscribe: () => () => undefined, sendMessage: () => 'request',
        }));
        await waitFor(() => expect(result.current.hydrating).toBe(false));
        expect(result.current.messages).toEqual([
            expect.objectContaining({ id: 'tool-call-command-1', role: 'tool_call', toolCallId: 'command-1' }),
            { id: 'waiting', role: 'assistant', text: 'Waiting for the background command.' },
            { id: 'finished', role: 'tool_result', toolCallId: 'command-1', text: output },
        ]);
        expect(result.current.tools[0].status).toBe(output.startsWith('execution failed:') ? 'failed' : 'ok');
        expect(result.current.status).toBe('done');
    });

    it.each(['update_text_file', 'delete_text_file'])('renders a successful %s recovery receipt outside the folded tool payload', async (name) => {
        const output = JSON.stringify({ result: 'verified', output: { kind: 'text_file_mutation', value: {
            operation: name === 'update_text_file' ? 'update' : 'delete', verified: true,
            original_file_name: 'notes.txt', original_size_bytes: 3, original_sha256: 'a'.repeat(64), updated_file: null,
            recovery: { recovery_id: 'b'.repeat(64), created_at_unix_ms: 1000, expires_at_unix_ms: 2000, cleanup_pending: false },
        } } });
        localStorage.setItem('ai-assistant-conversation:desk-1', 'conversation-1');
        vi.stubGlobal('fetch', vi.fn(async () => ({ ok: true, json: async () => ({ data: {
            ...snapshotFields,
            sessionId: 'session-1', seq: 34, active: false, messages: [
                { id: 'call', role: 'assistant', text: '', toolCalls: [{ id: 'file-1', name, argumentsJson: '{}' }] },
                { id: 'finished', role: 'tool', toolCallId: 'file-1', text: output },
            ],
        } }) })));
        const { result } = renderHook(() => useAiAssistantChat({
            deskId: 'desk-1', subscribe: () => () => undefined, sendMessage: () => 'request',
        }));
        await waitFor(() => expect(result.current.hydrating).toBe(false));
        expect(result.current.messages).toEqual([
            expect.objectContaining({ role: 'tool_call', toolCallId: 'file-1' }),
            { id: 'finished', role: 'tool_result', toolCallId: 'file-1', text: output },
        ]);
        expect(result.current.tools[0].status).toBe('ok');
    });

    it('attaches only the opaque owner-selected window reference', () => {
        const sendMessage = vi.fn(() => 'window-request-1');
        const { result } = renderHook(() => useAiAssistantChat({
            deskId: 'desk-1',
            subscribe: () => () => undefined,
            sendMessage,
        }));
        const objectRef = {
            token: 'opaque-window',
            snapshot_id: 'worker-1:7',
            object_kind: 'window' as const,
            expires_at: '2030-01-01T00:00:00Z',
        };

        act(() => {
            expect(result.current.attachWindow(objectRef, 'Calculator')).toBe(true);
        });
        expect(sendMessage).toHaveBeenLastCalledWith(
            SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_OBJECT_CONTEXT,
            expect.objectContaining({
                conversation_target: 'fixed_device',
                operation: {
                    kind: 'attach_window',
                    object_ref: objectRef,
                    display_summary: 'Calculator',
                },
            }),
            'desk-1',
        );
        const payload = sendMessage.mock.calls.at(-1)?.[1] as Record<string, unknown>;
        expect(JSON.stringify(payload)).not.toMatch(/handle|process_id|coordinate/);
    });

    it('prepends an older page only for the same durable snapshot', async () => {
        localStorage.setItem('ai-assistant-conversation:desk-1', 'conversation-1');
        const fetchMock = vi.fn()
            .mockResolvedValueOnce({
                ok: true,
                json: async () => ({ data: {
                    ...snapshotFields,
                    sessionId: 'session-1',
                    seq: 7,
                    active: false,
                    messages: [
                        { id: 'm2', role: 'user', text: 'newer question' },
                        { id: 'm3', role: 'assistant', text: 'newer answer' },
                    ],
                    messagePage: { hasMore: true, nextBeforeMessageId: 'm2', limit: 100 },
                    contextAttachments: [],
                } }),
            })
            .mockResolvedValueOnce({
                ok: true,
                json: async () => ({ data: {
                    ...snapshotFields,
                    sessionId: 'session-1',
                    seq: 7,
                    active: false,
                    messages: [
                        { id: 'm1', role: 'assistant', text: 'older answer' },
                        { id: 'm2', role: 'user', text: 'newer question' },
                    ],
                    messagePage: { hasMore: false, limit: 100 },
                    contextAttachments: [],
                } }),
            });
        stubSnapshotFetch(fetchMock);
        const { result } = renderHook(() => useAiAssistantChat({
            deskId: 'desk-1',
            subscribe: () => () => undefined,
            sendMessage: () => 'request',
        }));

        await waitFor(() => expect(result.current.hydrating).toBe(false));
        expect(result.current.hasMoreMessages).toBe(true);
        await act(async () => { await result.current.loadOlderMessages(); });

        expect(fetchMock.mock.calls[1]?.[0]).toContain('message_before=m2');
        expect(result.current.messages.map((message) => message.id)).toEqual(['m1', 'm2', 'm3']);
        expect(result.current.hasMoreMessages).toBe(false);
    });

    it('keeps expanded history and its cursor across live snapshot refreshes', async () => {
        vi.useFakeTimers();
        localStorage.setItem('ai-assistant-conversation:desk-1', 'conversation-1');
        const response = (seq: number, ids: string[], hasMore: boolean, cursor?: string) => ({
            ok: true,
            json: async () => ({ data: {
                ...snapshotFields,
                sessionId: 'session-1', seq, active: false,
                messages: ids.map(id => ({ id, role: 'assistant', text: `${id} at ${seq}` })),
                messagePage: { hasMore, nextBeforeMessageId: cursor },
            } }),
        });
        const fetchMock = vi.fn()
            .mockResolvedValueOnce(response(7, ['m3', 'm4'], true, 'm3'))
            .mockResolvedValueOnce(response(7, ['m2', 'm3'], true, 'm2'))
            .mockResolvedValueOnce(response(8, ['m4', 'm5'], true, 'm4'))
            .mockResolvedValueOnce(response(8, ['m1', 'm2'], false))
            .mockResolvedValue(response(9, ['m4', 'm5'], true, 'm4'));
        stubSnapshotFetch(fetchMock);
        const { result, unmount } = renderHook(() => useAiAssistantChat({
            deskId: 'desk-1', subscribe: () => () => undefined, sendMessage: () => 'request',
        }));
        await act(async () => { await Promise.resolve(); });
        await act(async () => { await result.current.loadOlderMessages(); });
        await act(async () => { await vi.advanceTimersByTimeAsync(2_000); });
        expect(result.current.messages.map(message => message.id)).toEqual(['m2', 'm3', 'm4', 'm5']);
        expect(result.current.messages.find(message => message.id === 'm4')?.text).toBe('m4 at 8');
        await act(async () => { await result.current.loadOlderMessages(); });
        expect(fetchMock.mock.calls[3]?.[0]).toContain('message_before=m2');
        await act(async () => { await vi.advanceTimersByTimeAsync(2_000); });
        expect(result.current.messages.map(message => message.id)).toEqual(['m1', 'm2', 'm3', 'm4', 'm5']);
        expect(result.current.hasMoreMessages).toBe(false);
        unmount();
    });

    it.each([false, true])('handles a pending history page when conversation changes: %s', async (switchConversation) => {
        vi.useFakeTimers();
        localStorage.setItem('ai-assistant-conversation:desk-1', 'conversation-1');
        const response = (sessionId: string, seq: number, ids: string[]) => ({
            ok: true,
            json: async () => ({ data: {
                ...snapshotFields, sessionId, seq, active: false,
                messages: ids.map(id => ({ id, role: 'assistant', text: `${id} at ${seq}` })),
                messagePage: { hasMore: true, nextBeforeMessageId: ids[0] },
            } }),
        });
        let resolveOlder!: (value: ReturnType<typeof response>) => void;
        const fetchMock = vi.fn()
            .mockResolvedValueOnce(response('session-1', 7, ['m2']))
            .mockImplementationOnce(() => new Promise(resolve => { resolveOlder = resolve; }))
            .mockResolvedValue(response(switchConversation ? 'session-2' : 'session-1', 8, ['m2', 'm3']));
        stubSnapshotFetch(fetchMock);
        const { result, unmount } = renderHook(() => useAiAssistantChat({
            deskId: 'desk-1', subscribe: () => () => undefined, sendMessage: () => 'request',
        }));
        await act(async () => { await Promise.resolve(); });
        let loading!: Promise<void>;
        act(() => { loading = result.current.loadOlderMessages(); });
        if (switchConversation) {
            await act(async () => { expect(result.current.selectConversation('conversation-2')).toBe(true); });
        } else {
            await act(async () => { await vi.advanceTimersByTimeAsync(2_000); });
        }
        await act(async () => {
            resolveOlder(response('session-1', 7, ['m1', 'm2']));
            await loading;
        });
        expect(result.current.messages.map(message => message.id)).toEqual(
            switchConversation ? ['m2', 'm3'] : ['m1', 'm2', 'm3'],
        );
        expect(result.current.messages.find(message => message.id === 'm2')?.text).toBe('m2 at 8');
        expect(result.current.loadingOlderMessages).toBe(false);
        unmount();
    });

    it('ignores an older durable snapshot that resolves after a newer one', async () => {
        vi.useFakeTimers();
        localStorage.setItem('ai-assistant-conversation:desk-1', 'conversation-1');
        const pending: Array<(response: {
            ok: boolean;
            json: () => Promise<unknown>;
        }) => void> = [];
        stubSnapshotFetch(vi.fn(() => new Promise((resolve) => pending.push(resolve))));
        const { result } = renderHook(() => useAiAssistantChat({
            deskId: 'desk-1',
            subscribe: () => () => undefined,
            sendMessage: () => 'request',
        }));

        await act(async () => {
            await Promise.resolve();
        });
        expect(pending).toHaveLength(1);
        await act(async () => {
            await vi.advanceTimersByTimeAsync(2_000);
        });
        expect(pending).toHaveLength(2);

        await act(async () => {
            pending[1]({
                ok: true,
                json: async () => ({
                    data: {
                        ...snapshotFields,
                        sessionId: 'session-1',
                        seq: 12,
                        active: false,
                        messages: [{ id: 'answer-12', role: 'assistant', text: 'newest answer' }],
                        contextNotices: [{ id: 'new-notice', turnId: 'new-turn', kind: 'compacted' }],
                        contextAttachments: [],
                        backgroundTasks: [{
                            taskId: 'task-1',
                            callId: 'call-1',
                            providerId: 'browser.control',
                            capabilityId: 'browser.control.semantic',
                            toolName: 'browser_take_snapshot',
                            effect: 'read_device',
                            state: 'succeeded',
                            progressSequence: 12,
                            supportsCancel: false,
                            updatedAt: '2026-08-31T00:00:12Z',
                        }],
                    },
                }),
            });
            await Promise.resolve();
            await Promise.resolve();
        });
        expect(result.current.messages.at(-1)?.text).toBe('newest answer');
        expect(result.current.backgroundTasks[0]?.state).toBe('succeeded');

        await act(async () => {
            pending[0]({
                ok: true,
                json: async () => ({
                    data: {
                        ...snapshotFields,
                        sessionId: 'session-1',
                        seq: 11,
                        active: true,
                        messages: [{ id: 'answer-11', role: 'assistant', text: 'older answer' }],
                        contextNotices: [{ id: 'old-notice', turnId: 'old-turn', kind: 'trimmed' }],
                        contextAttachments: [],
                        backgroundTasks: [{
                            taskId: 'task-1',
                            callId: 'call-1',
                            providerId: 'browser.control',
                            capabilityId: 'browser.control.semantic',
                            toolName: 'browser_take_snapshot',
                            effect: 'read_device',
                            state: 'running',
                            progressSequence: 11,
                            supportsCancel: false,
                            updatedAt: '2026-08-31T00:00:11Z',
                        }],
                    },
                }),
            });
            await Promise.resolve();
            await Promise.resolve();
        });

        expect(result.current.messages.at(-1)?.text).toBe('newest answer');
        expect(result.current.backgroundTasks[0]?.state).toBe('succeeded');
        expect(result.current.running).toBe(false);
        expect(result.current.contextNotices.map(notice => notice.id)).toEqual(['new-notice']);
    });

    it('ignores a tied goal snapshot that resolves after a later request', async () => {
        vi.useFakeTimers();
        localStorage.setItem('ai-assistant-conversation:desk-1', 'conversation-1');
        const pending: Array<(response: {
            ok: boolean;
            json: () => Promise<unknown>;
        }) => void> = [];
        stubSnapshotFetch(vi.fn(() => new Promise((resolve) => pending.push(resolve))));
        const { result } = renderHook(() => useAiAssistantChat({
            deskId: 'desk-1',
            subscribe: () => () => undefined,
            sendMessage: () => 'request',
        }));

        await act(async () => {
            await Promise.resolve();
        });
        expect(pending).toHaveLength(1);
        await act(async () => {
            await vi.advanceTimersByTimeAsync(2_000);
        });
        expect(pending).toHaveLength(2);

        await act(async () => {
            pending[1]({
                ok: true,
                json: async () => ({
                    data: {
                        ...snapshotFields,
                        sessionId: 'session-1',
                        seq: 12,
                        active: false,
                        messages: [{ id: 'answer-12', role: 'assistant', text: 'newest answer' }],
                        contextNotices: [{ id: 'new-notice', turnId: 'new-turn', kind: 'compacted' }],
                        contextAttachments: [],
                        backgroundTasks: [{
                            taskId: 'task-1',
                            callId: 'call-1',
                            providerId: 'browser.control',
                            capabilityId: 'browser.control.semantic',
                            toolName: 'browser_take_snapshot',
                            effect: 'read_device',
                            state: 'succeeded',
                            progressSequence: 12,
                            supportsCancel: false,
                            updatedAt: '2026-08-31T00:00:12Z',
                        }],
                    },
                }),
            });
            await Promise.resolve();
            await Promise.resolve();
        });
        expect(result.current.messages.at(-1)?.text).toBe('newest answer');
        expect(result.current.backgroundTasks[0]?.state).toBe('succeeded');

        await act(async () => {
            pending[0]({
                ok: true,
                json: async () => ({
                    data: {
                        ...snapshotFields,
                        sessionId: 'session-1',
                        seq: 12,
                        active: true,
                        goal: { goalId: 'obsolete', state: 'running', goalText: 'Old goal' },
                        messages: [{ id: 'answer-11', role: 'assistant', text: 'older answer' }],
                        contextNotices: [{ id: 'old-notice', turnId: 'old-turn', kind: 'trimmed' }],
                        contextAttachments: [],
                        backgroundTasks: [{
                            taskId: 'task-1',
                            callId: 'call-1',
                            providerId: 'browser.control',
                            capabilityId: 'browser.control.semantic',
                            toolName: 'browser_take_snapshot',
                            effect: 'read_device',
                            state: 'running',
                            progressSequence: 11,
                            supportsCancel: false,
                            updatedAt: '2026-08-31T00:00:11Z',
                        }],
                    },
                }),
            });
            await Promise.resolve();
            await Promise.resolve();
        });

        expect(result.current.messages.at(-1)?.text).toBe('newest answer');
        expect(result.current.backgroundTasks[0]?.state).toBe('succeeded');
        expect(result.current.running).toBe(false);
        expect(result.current.goal).toBeNull();
        expect(result.current.contextNotices.map(notice => notice.id)).toEqual(['new-notice']);
    });

    it('isolates an old request after the selected conversation changes', async () => {
        localStorage.setItem('ai-assistant-conversation:desk-1', 'conversation-1');
        const pending: Array<(response: {
            ok: boolean;
            json: () => Promise<unknown>;
        }) => void> = [];
        stubSnapshotFetch(vi.fn(() => new Promise((resolve) => pending.push(resolve))));
        const { result } = renderHook(() => useAiAssistantChat({
            deskId: 'desk-1',
            subscribe: () => () => undefined,
            sendMessage: () => 'request',
        }));

        await act(async () => {
            await Promise.resolve();
        });
        expect(pending).toHaveLength(1);

        await act(async () => {
            window.dispatchEvent(new StorageEvent('storage', {
                key: 'ai-assistant-conversation:desk-1',
                newValue: 'conversation-2',
            }));
            await Promise.resolve();
        });
        expect(pending).toHaveLength(2);

        await act(async () => {
            pending[1]({
                ok: true,
                json: async () => ({
                    data: {
                        ...snapshotFields,
                        sessionId: 'session-2',
                        seq: 1,
                        active: false,
                        messages: [{ id: 'new', role: 'assistant', text: 'new conversation' }],
                        contextAttachments: [],
                    },
                }),
            });
            await Promise.resolve();
            await Promise.resolve();
        });
        expect(result.current.messages.at(-1)?.text).toBe('new conversation');

        await act(async () => {
            pending[0]({
                ok: true,
                json: async () => ({
                    data: {
                        ...snapshotFields,
                        sessionId: 'session-1',
                        seq: 99,
                        active: false,
                        messages: [{ id: 'old', role: 'assistant', text: 'old conversation' }],
                        contextAttachments: [],
                    },
                }),
            });
            await Promise.resolve();
            await Promise.resolve();
        });
        expect(result.current.messages.at(-1)?.text).toBe('new conversation');
    });

    it('lets a later request move the watermark to a replacement server session', async () => {
        vi.useFakeTimers();
        localStorage.setItem('ai-assistant-conversation:desk-1', 'conversation-1');
        const pending: Array<(response: {
            ok: boolean;
            json: () => Promise<unknown>;
        }) => void> = [];
        stubSnapshotFetch(vi.fn(() => new Promise((resolve) => pending.push(resolve))));
        const { result } = renderHook(() => useAiAssistantChat({
            deskId: 'desk-1',
            subscribe: () => () => undefined,
            sendMessage: () => 'request',
        }));

        await act(async () => {
            await Promise.resolve();
            await vi.advanceTimersByTimeAsync(2_000);
        });
        expect(pending).toHaveLength(2);

        await act(async () => {
            pending[0]({
                ok: true,
                json: async () => ({
                    data: {
                        ...snapshotFields,
                        sessionId: 'expired-session',
                        seq: 40,
                        active: false,
                        messages: [{ id: 'old', role: 'assistant', text: 'expired session' }],
                        contextAttachments: [],
                    },
                }),
            });
            await Promise.resolve();
            await Promise.resolve();
        });
        expect(result.current.messages.at(-1)?.text).toBe('expired session');

        await act(async () => {
            pending[1]({
                ok: true,
                json: async () => ({
                    data: {
                        ...snapshotFields,
                        sessionId: 'replacement-session',
                        seq: 1,
                        active: false,
                        messages: [{ id: 'new', role: 'assistant', text: 'replacement session' }],
                        contextAttachments: [],
                    },
                }),
            });
            await Promise.resolve();
            await Promise.resolve();
        });
        expect(result.current.messages.at(-1)?.text).toBe('replacement session');
    });

    it('discovers another tab conversation and polls its durable completion', async () => {
        vi.useFakeTimers();
        let snapshot = {
            ...snapshotFields,
            sessionId: 'session-1',
            seq: 1,
            active: true,
            messages: [{ id: 'user-1', role: 'user', text: 'shared request' }],
            contextAttachments: [],
            backgroundTasks: [{
                taskId: 'task-1',
                callId: 'call-1',
                providerId: 'office.document',
                capabilityId: 'office.document.inspect',
                toolName: 'inspect_excel_selection',
                effect: 'read_device',
                state: 'running',
                progressSequence: 2,
                supportsCancel: true,
                updatedAt: '2026-08-26T00:00:00Z',
            }],
        };
        vi.stubGlobal('fetch', vi.fn(async () => ({
            ok: true,
            json: async () => ({ data: snapshot }),
        })));
        const { result } = renderHook(() => useAiAssistantChat({
            deskId: 'desk-1',
            subscribe: () => () => undefined,
            sendMessage: () => 'request',
        }));

        await act(async () => {
            window.dispatchEvent(new StorageEvent('storage', {
                key: 'ai-assistant-conversation:desk-1',
                newValue: 'shared-conversation',
            }));
            await Promise.resolve();
            await Promise.resolve();
        });
        expect(result.current.running).toBe(true);
        expect(result.current.messages.at(-1)?.text).toBe('shared request');
        expect(result.current.backgroundTasks).toEqual([
            expect.objectContaining({ taskId: 'task-1', state: 'running', progressSequence: 2 }),
        ]);

        snapshot = {
            active: false,
            ...snapshotFields,
            sessionId: 'session-1',
            seq: 2,
            messages: [
                { id: 'user-1', role: 'user', text: 'shared request' },
                { id: 'assistant-1', role: 'assistant', text: 'shared answer' },
            ],
            contextAttachments: [],
            backgroundTasks: [{
                taskId: 'task-1',
                callId: 'call-1',
                providerId: 'office.document',
                capabilityId: 'office.document.inspect',
                toolName: 'inspect_excel_selection',
                effect: 'read_device',
                state: 'succeeded',
                progressSequence: 3,
                supportsCancel: true,
                updatedAt: '2026-08-26T00:00:02Z',
                terminalAt: '2026-08-26T00:00:02Z',
            }],
        };
        await act(async () => {
            await vi.advanceTimersByTimeAsync(2_000);
        });
        expect(result.current.running).toBe(false);
        expect(result.current.status).toBe('done');
        expect(result.current.messages.at(-1)?.text).toBe('shared answer');
        expect(result.current.backgroundTasks).toEqual([
            expect.objectContaining({ taskId: 'task-1', state: 'succeeded', progressSequence: 3 }),
        ]);
    });

    it('keeps two mounted pages on one durable snapshot and closing one does not cancel the run', async () => {
        vi.useFakeTimers();
        localStorage.setItem('ai-assistant-conversation:device-1', 'shared-conversation');
        let snapshot = {
            ...snapshotFields,
            sessionId: 'session-1',
            seq: 1,
            active: true,
            latestInputSeq: 3,
            handledInputSeq: 2,
            messages: [{ id: 'user-1', role: 'user', text: 'shared request' }],
            contextAttachments: [],
            permissionRequests: [],
            backgroundTasks: [{
                taskId: 'task-1',
                callId: 'call-1',
                providerId: 'browser.control',
                capabilityId: 'browser.control.semantic',
                toolName: 'prepare_gmail_draft',
                effect: 'write_external_draft',
                state: 'running',
                progressSequence: 4,
                supportsCancel: true,
                updatedAt: '2026-08-29T00:00:00Z',
            }],
            capabilityGrants: [{
                grantId: 'grant-1',
                providerId: 'browser.control',
                capabilityId: 'browser.control.semantic',
                toolName: 'prepare_gmail_draft',
                riskTier: 'r2',
                resourceScope: ['browser-extension-surface'],
                operationScope: ['write_external_draft'],
                remainingUses: 1,
                expiresAtUnixMs: 999,
                revokedAtUnixMs: null,
            }],
            unresolvedOutcome: null,
        };
        vi.stubGlobal('fetch', vi.fn(async () => ({
            ok: true,
            json: async () => ({ data: snapshot }),
        })));
        const firstSend = vi.fn(() => 'first-request');
        const secondSend = vi.fn(() => 'second-request');
        const first = renderHook(() => useAiAssistantChat({
            deskId: 'connection-1',
            conversationStorageScope: 'device-1',
            subscribe: () => () => undefined,
            sendMessage: firstSend,
        }));
        const second = renderHook(() => useAiAssistantChat({
            deskId: 'connection-1',
            conversationStorageScope: 'device-1',
            subscribe: () => () => undefined,
            sendMessage: secondSend,
        }));
        await act(async () => {
            await Promise.resolve();
            await Promise.resolve();
        });

        expect(first.result.current.messages).toEqual(second.result.current.messages);
        expect(first.result.current.backgroundTasks).toEqual(second.result.current.backgroundTasks);
        expect(first.result.current.capabilityGrants).toEqual(second.result.current.capabilityGrants);
        expect(first.result.current.pendingInputCount).toBe(1);
        expect(second.result.current.pendingInputCount).toBe(1);

        first.unmount();
        snapshot = {
            ...snapshot,
            seq: 2,
            active: false,
            handledInputSeq: 3,
            messages: [
                snapshot.messages[0],
                { id: 'assistant-1', role: 'assistant', text: 'shared answer' },
            ],
            backgroundTasks: [{
                ...snapshot.backgroundTasks[0],
                state: 'succeeded',
                progressSequence: 5,
                terminalAt: '2026-08-29T00:00:02Z',
            }],
        };
        await act(async () => {
            await vi.advanceTimersByTimeAsync(2_000);
        });

        expect(firstSend).not.toHaveBeenCalled();
        expect(secondSend).not.toHaveBeenCalled();
        expect(second.result.current.running).toBe(false);
        expect(second.result.current.status).toBe('done');
        expect(second.result.current.messages.at(-1)?.text).toBe('shared answer');
        expect(second.result.current.pendingInputCount).toBe(0);
        expect(second.result.current.backgroundTasks[0]).toEqual(
            expect.objectContaining({ state: 'succeeded', progressSequence: 5 }),
        );
    });

    it('persists context independently and blocks a turn until the ack arrives', async () => {
        let subscriber: SignalingSubscriber | null = null;
        vi.stubGlobal('fetch', vi.fn(async () => ({
            ok: true,
            json: async () => ({
                data: {
                    ...snapshotFields,
                    sessionId: 'session-1',
                    seq: 1,
                    active: false,
                    messages: [],
                    contextAttachments: [{
                        id: 'attachment-1',
                        kind: 'interactive_session',
                        providerId: 'desktop.session',
                        capabilityId: 'desktop.session.inspect',
                        displaySummary: 'current interactive session',
                        createdAtUnixMs: 100,
                        expiresAtUnixMs: 200,
                        state: 'active',
                    }],
                },
            }),
        })));
        const sendMessage = vi.fn((type: number) => type ===
            SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_CONTEXT
            ? 'context-request-1'
            : 'assistant-request-1');
        const { result } = renderHook(() => useAiAssistantChat({
            deskId: 'desk-1',
            subscribe: (handler) => {
                subscriber = handler;
                return () => { subscriber = null; };
            },
            sendMessage,
        }));

        act(() => {
            expect(result.current.updateContext(['desktop.session.inspect'])).toBe(true);
        });
        expect(result.current.contextUpdating).toBe(true);
        expect(result.current.start('must wait')).toBe(false);
        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_CONTEXT,
            expect.objectContaining({
                client_request_id: expect.any(String),
                selected_capability_ids: ['desktop.session.inspect'],
            }),
            'desk-1',
        );

        act(() => {
            subscriber?.({
                request_id: 'context-request-1',
                signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_CONTEXT_UPDATED,
                signaling_data: { changed: true },
            });
        });
        await waitFor(() => expect(result.current.contextUpdating).toBe(false));
        await waitFor(() => expect(result.current.attachments).toEqual([
            expect.objectContaining({ capabilityId: 'desktop.session.inspect', state: 'active' }),
        ]));
        act(() => {
            expect(result.current.start('now continue')).toBe(true);
        });
    });

    it('detaches object context explicitly and sends only persisted attachment ids', async () => {
        localStorage.setItem('ai-assistant-conversation:desk-1', 'conversation-1');
        let subscriber: SignalingSubscriber | null = null;
        vi.stubGlobal('fetch', vi.fn(async () => ({
            ok: true,
            json: async () => ({
                data: {
                    ...snapshotFields,
                    sessionId: 'session-1',
                    seq: 1,
                    active: false,
                    messages: [],
                    contextAttachments: [{
                        id: 'file-attachment-1',
                        kind: 'file',
                        providerId: 'file.workspace',
                        capabilityId: 'file.metadata.read',
                        displaySummary: 'selected.txt',
                        createdAtUnixMs: 100,
                        expiresAtUnixMs: Date.now() + 60_000,
                        state: 'active',
                    }],
                },
            }),
        })));
        const sendMessage = vi.fn((type: number) => type ===
            SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_OBJECT_CONTEXT
            ? 'object-context-request-1'
            : 'assistant-request-1');
        const { result } = renderHook(() => useAiAssistantChat({
            deskId: 'desk-1',
            subscribe: (handler) => {
                subscriber = handler;
                return () => { subscriber = null; };
            },
            sendMessage,
        }));

        await waitFor(() => expect(result.current.attachments).toHaveLength(1));
        act(() => {
            expect(result.current.detachAttachment('file-attachment-1')).toBe(true);
        });
        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_OBJECT_CONTEXT,
            expect.objectContaining({
                conversation_id: 'conversation-1',
                operation: { kind: 'detach', attachment_id: 'file-attachment-1' },
            }),
            'desk-1',
        );
        act(() => {
            subscriber?.({
                request_id: 'object-context-request-1',
                signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_OBJECT_CONTEXT_UPDATED,
                signaling_data: { changed: true },
            });
        });
        await waitFor(() => expect(result.current.contextUpdating).toBe(false));

        act(() => {
            expect(result.current.start('Read the selected metadata.')).toBe(true);
        });
        expect(sendMessage).toHaveBeenLastCalledWith(
            SIGNALING_TYPE_CODE_ASK_AI_ASSISTANT,
            expect.objectContaining({ selected_attachment_ids: ['file-attachment-1'] }),
            'desk-1',
            expect.any(String),
        );
    });

    it('reads the saved preview and answer after the stream signals completion', async () => {
        let subscriber: SignalingSubscriber | null = null;
        const subscribe = (handler: SignalingSubscriber) => {
            subscriber = handler;
            return () => { subscriber = null; };
        };
        const sendMessage = vi.fn((_type: number, _payload: { client_message_id: string }) => 'assistant-request-1');
        const { result } = renderHook(() => useAiAssistantChat({
            deskId: 'desk-1',
            subscribe,
            sendMessage,
        }));

        act(() => {
            expect(result.current.start(
                'Inspect Word and prepare a focus preview.',
                'en-US',
                ['desktop.ui.inspect'],
            )).toBe(true);
        });
        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_ASK_AI_ASSISTANT,
            expect.objectContaining({
                question: 'Inspect Word and prepare a focus preview.',
                locale: 'en-US',
                selected_capability_ids: ['desktop.ui.inspect'],
            }),
            'desk-1',
            expect.any(String),
        );

        const draft = {
            schema_version: 1,
            adapter: { kind: 'windows_uia', version: 'a4-windows-uia-read/v1' },
            risk: 'low',
            reversible: true,
            data_egress: false,
            actions: [{
                target: {
                    token: 'opaque',
                    snapshot_id: 'snapshot-1',
                    object_kind: 'ui_element',
                    expires_at: '2026-08-24T00:00:00Z',
                },
                action: { adapter: 'ui', action: { kind: 'focus' } },
                before_summary: 'Document surface is not focused.',
                after_intent: 'Focus the document surface.',
                verification: 'Inspect focus state again.',
            }],
        };
        const input = sendMessage.mock.calls[0][1];
        stubSnapshotFetch(vi.fn(async () => ({ ok: true, json: async () => ({ data: {
            ...snapshotFields, sessionId: 'preview-session', requestId: 'assistant-request-1', seq: 3, active: false,
            messages: [
                { id: input.client_message_id, role: 'user', text: 'Inspect Word and prepare a focus preview.' },
                { id: 'reasoning', role: 'assistant', text: '', reasoning: 'Preview the change first.', toolCalls: [
                    { id: 'draft-1', name: 'preview_computer_action', argumentsJson: JSON.stringify(draft) },
                ] },
                { id: 'preview-output', role: 'tool', text: JSON.stringify(draft), toolCallId: 'draft-1', toolOk: true },
                { id: 'saved-answer', role: 'assistant', text: 'I prepared a focus preview. Nothing was executed.' },
            ],
        } }) })));
        await act(async () => {
            subscriber?.({
                request_id: 'assistant-request-1',
                signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED,
                signaling_data: {
                    request_id: 'assistant-request-1',
                    seq: 0,
                    kind: 'status',
                    status: 'accepted',
                },
            });
            subscriber?.({
                request_id: 'assistant-request-1',
                signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED,
                signaling_data: {
                    request_id: 'assistant-request-1',
                    seq: 1,
                    kind: 'tool_started',
                    tool_name: 'preview_computer_action',
                    tool_call_id: 'draft-1',
                    tool_arguments_json: JSON.stringify(draft),
                },
            });
            subscriber?.({
                request_id: 'assistant-request-1',
                signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED,
                signaling_data: {
                    request_id: 'assistant-request-1',
                    seq: 2,
                    kind: 'tool_finished',
                    tool_call_id: 'draft-1',
                    tool_ok: true,
                    tool_output: JSON.stringify(draft),
                },
            });
            subscriber?.({
                request_id: 'assistant-request-1',
                signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED,
                signaling_data: {
                    request_id: 'assistant-request-1',
                    seq: 3,
                    kind: 'answer',
                    answer: 'I prepared a focus preview. Nothing was executed.',
                },
            });
        });

        expect(result.current.messages.map(message => message.id)).toEqual([
            input.client_message_id, 'reasoning', 'tool-call-draft-1', 'saved-answer',
        ]);
        expect(result.current.tools.find(tool => tool.callId === 'draft-1')).toMatchObject({
            name: 'preview_computer_action', argumentsJson: JSON.stringify(draft), output: JSON.stringify(draft), status: 'ok',
        });
        expect(result.current.draft?.actions).toHaveLength(1);
        expect(result.current.messages.at(-1)?.text).toContain('Nothing was executed');
        expect(result.current.running).toBe(false);
        expect(result.current.status).toBe('done');
    });

    it('deduplicates visual evidence and keeps its live preview out of transcript state', () => {
        let subscriber: SignalingSubscriber | null = null;
        const { result } = renderHook(() => useAiAssistantChat({
            deskId: 'desk-1',
            subscribe: (handler) => {
                subscriber = handler;
                return () => { subscriber = null; };
            },
            sendMessage: () => 'assistant-request-1',
        }));
        act(() => { expect(result.current.start('Look at the screen.')).toBe(true); });
        const evidence = {
            schema_version: 1,
            evidence_id: 'evidence-1',
            conversation_id: 'conversation-1',
            focus_input_revision: 1,
            turn_id: 'turn-1',
            tool_call_id: 'call-1',
            frame_id: 'frame-1',
            phase: 'observation' as const,
            status: 'available' as const,
            captured_at_unix_ms: 1,
            expires_at_unix_ms: 2,
            device_id: 'desk-1',
            content: null,
            digest_sha256: 'a'.repeat(64),
            size_bytes: 3,
            media_type: 'image/png',
            preview_data_url: 'data:image/png;base64,AQID',
        };
        act(() => {
            subscriber?.({
                request_id: 'assistant-request-1',
                signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED,
                signaling_data: {
                    request_id: 'assistant-request-1', seq: 0, kind: 'visual_evidence',
                    visual_evidence: evidence,
                },
            });
            subscriber?.({
                request_id: 'assistant-request-1',
                signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED,
                signaling_data: {
                    request_id: 'assistant-request-1', seq: 1, kind: 'visual_evidence',
                    visual_evidence: {
                        ...evidence, status: 'not_retained', preview_data_url: null,
                    },
                },
            });
        });
        expect(result.current.visualEvidence).toHaveLength(1);
        expect(result.current.visualEvidence[0]?.preview_data_url).toBe(evidence.preview_data_url);
        expect(result.current.messages).toHaveLength(1);
        expect(result.current.messages[0]?.text).toBe('Look at the screen.');
        act(() => { result.current.reset(); });
        expect(result.current.visualEvidence).toEqual([]);
    });

    it('ignores a stale assistant stream', () => {
        let subscriber: SignalingSubscriber | null = null;
        const { result } = renderHook(() => useAiAssistantChat({
            deskId: 'desk-1',
            subscribe: (handler) => {
                subscriber = handler;
                return () => { subscriber = null; };
            },
            sendMessage: () => 'current',
        }));
        act(() => { result.current.start('Inspect the current UI.'); });
        act(() => {
            subscriber?.({
                request_id: 'stale',
                signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED,
                signaling_data: {
                    request_id: 'stale',
                    seq: 99,
                    kind: 'answer',
                    answer: 'stale answer',
                },
            });
        });
        expect(result.current.messages.some((message) => message.text === 'stale answer')).toBe(false);
        expect(result.current.running).toBe(true);
    });

    it('restores ordered tool calls including tool-only turns, errors, and missing results', async () => {
        localStorage.setItem('ai-assistant-conversation:tool-records', 'saved-conversation');
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: {
            ...snapshotFields,
            sessionId: 'saved-conversation', seq: 3, active: false, messages: [
                { id: 'user', role: 'user', text: 'check' },
                { id: 'calls', role: 'assistant', text: '', toolCalls: [
                    { id: 'a', name: 'inspect_desktop_ui', argumentsJson: '{"query":{"name":"Calendar"}}' },
                    { id: 'b', name: 'load_tool_details', argumentsJson: '{}' },
                ] },
                { id: 'result', role: 'tool', toolCallId: 'a', text: 'tool error: permission required' },
                { id: 'answer', role: 'assistant', text: 'stopped' },
            ],
        } }) }));
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'tool-records',
            subscribe: () => () => undefined, sendMessage: () => 'unused' }));
        await waitFor(() => expect(result.current.messages).toHaveLength(4));
        expect(result.current.messages.map(message => message.id)).toEqual(['user', 'tool-call-a', 'tool-call-b', 'answer']);
        expect(result.current.tools[0]).toMatchObject({ name: 'inspect_desktop_ui', status: 'failed', output: 'tool error: permission required' });
        expect(result.current.tools[1].output).toBeNull();
        unmount();
    });

    it('restores reasoning on tool-only assistant messages and final answers', async () => {
        localStorage.setItem('ai-assistant-conversation:reasoning', 'saved-conversation');
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: {
            ...snapshotFields,
            sessionId: 'saved-conversation', seq: 3, active: false, messages: [
                { id: 'user', role: 'user', text: 'check', reasoning: 'ignore' },
                { id: 'thinking', role: 'assistant', text: '', reasoning: 'Inspect first.' },
                { id: 'answer', role: 'assistant', text: 'all good', reasoning: 'Evidence is sufficient.' },
            ],
        } }) }));
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'reasoning',
            subscribe: () => () => undefined, sendMessage: () => 'unused' }));
        await waitFor(() => expect(result.current.messages).toHaveLength(3));
        expect(result.current.messages.map(message => message.reasoning)).toEqual([undefined, 'Inspect first.', 'Evidence is sufficient.']);
        expect(result.current.messages[2].text).toBe('all good');
        unmount();
    });

    it('restores command tasks and cancels only the selected original generation', async () => {
        localStorage.setItem('ai-assistant-conversation:task-list', 'saved-conversation');
        const tasks = [
            { taskId: 'command-1', callId: 'call-1', executionGeneration: 'generation-1', state: 'running', updatedAt: '2026-09-08T00:00:00Z', result: null, resultTruncated: false },
            { taskId: 'command-2', callId: 'call-2', executionGeneration: 'generation-2', state: 'succeeded', updatedAt: '2026-09-08T00:00:00Z', result: 'original output', resultTruncated: false },
        ];
        const fetchMock = vi.fn().mockImplementation(async (url: string) => ({ ok: true, json: async () => url.endsWith('/command/cancel') ? ({ code: deskErrorCodeEnum.SUCCESS, data: true }) : ({ data: {
            ...snapshotFields,
            sessionId: 'saved-conversation', seq: 10, active: false, messages: [], commandTasks: tasks,
        } }) }));
        vi.stubGlobal('fetch', fetchMock);
        const sendMessage = vi.fn().mockReturnValue('control-request');
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'task-list',
            subscribe: () => () => undefined, sendMessage }));
        await waitFor(() => expect(result.current.commandTasks).toHaveLength(2));
        await act(async () => result.current.cancelTask('command', 'command-1'));
        const cancellations = fetchMock.mock.calls.filter(([url]) => url.endsWith('/command/cancel'));
        expect(cancellations).toHaveLength(1);
        expect(JSON.parse(cancellations[0][1].body)).toEqual({ connection: 'task-list', session: 'saved-conversation', exec_request_id: 'command-1', execution_generation: 'generation-1' });
        expect(result.current.commandTasks[0].state).toBe('running');
        expect(result.current.commandTasks[1].result).toBe('original output');
        await act(async () => { await expect(result.current.cancelTask('command', 'command-2')).rejects.toThrow(); });
        expect(fetchMock.mock.calls.filter(([url]) => url.endsWith('/command/cancel'))).toHaveLength(1);
        expect(sendMessage).not.toHaveBeenCalled();
        unmount();
    });

    it('streams the stable queued request while another submission awaits acknowledgement', async () => {
        let subscriber: SignalingSubscriber | null = null;
        let finished = false;
        const sendMessage = vi.fn((_type: number, _payload: unknown, _to?: string, id?: string) => id!);
        vi.stubGlobal('fetch', vi.fn(async (url: string) => ({ ok: true, json: async () => finished && !url.includes('/requests?')
            ? { data: { ...snapshotFields, sessionId: 'root', requestId: 'request-a', seq: 2, active: false, messages: [
                { id: (sendMessage.mock.calls[0][1] as { client_message_id: string }).client_message_id, role: 'user', text: 'A' },
                { id: 'answer-a', role: 'assistant', text: 'A done' },
            ] } }
            : { success: false, code: deskErrorCodeEnum.PERMISSION_ERROR, data: null },
        })));
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: '', connected: true,
            subscribe: handler => { subscriber = handler; return () => { subscriber = null; }; }, sendMessage }));
        const deliver = (requestId: string, data: Record<string, unknown>) => subscriber?.({
            request_id: requestId, signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED,
            signaling_data: { request_id: requestId, ...data },
        });
        const receipt = (submission: string, request: string) => deliver(submission, { seq: 1,
            kind: 'status', status: 'queued', main_request_id: request, main_request_session_id: 'root' });
        try {
            act(() => { expect(result.current.start('A')).toBe(true); });
            act(() => receipt(sendMessage.mock.calls[0][3]!, 'request-a'));
            act(() => {
                deliver('request-a', { seq: 0, kind: 'turn_started' });
                deliver('request-a', { seq: 1, kind: 'partial', partial_summary: 'Part ' });
            });
            act(() => { expect(result.current.start('B')).toBe(true); });
            act(() => deliver('request-a', { seq: 2, kind: 'partial', partial_summary: 'one' }));
            expect(result.current.partial).toBe('Part one');
            expect(result.current.deliveryState).toBe('sending');
            finished = true;
            await act(async () => deliver('request-a', { seq: 3, kind: 'answer', answer: 'A done' }));
            expect(result.current.deliveryState).toBe('sending');
            expect(result.current.messages.map(message => message.text)).toEqual(['A', 'A done', 'B']);
            act(() => receipt(sendMessage.mock.calls[1][3]!, 'request-b'));
            act(() => {
                deliver('request-a', { seq: 4, kind: 'partial', partial_summary: 'late' });
                deliver('request-b', { seq: 1, kind: 'status', status: 'accepted' });
                deliver('request-b', { seq: 2, kind: 'partial', partial_summary: 'B only' });
            });
            expect(result.current.partial).toBe('B only');
            expect(result.current.acceptedInput?.question).toBe('B');
        } finally { unmount(); }
    });

    it('keeps a queued stream through a pre-claim snapshot and shows the approval continuation live', async () => {
        let subscriber: SignalingSubscriber | null = null;
        let finished = false;
        const fetchMock = vi.fn(async (url: string, _options?: RequestInit) => ({
            ok: true, json: async () => ({ success: true, data: url.includes('/requests?')
                ? { sessionId: 'root', items: [] }
                : { ...snapshotFields, sessionId: 'root', seq: finished ? 2 : 1, active: false,
                    requestId: finished ? 'resume' : undefined,
                    messages: finished ? [{ id: 'saved-report', role: 'assistant', text: 'Disk report ready' }] : [] } }),
        }));
        vi.stubGlobal('fetch', fetchMock);
        const sendMessage = vi.fn((_type: number, _payload: unknown, _to?: string, id?: string) => id!);
        const { result } = renderHook(() => useAiAssistantChat({ deskId: '', connected: true,
            subscribe: handler => { subscriber = handler; return () => { subscriber = null; }; }, sendMessage }));
        const deliver = (id: string, data: Record<string, unknown>) => subscriber?.({
            request_id: id, signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED,
            signaling_data: { request_id: id, ...data },
        });
        act(() => { expect(result.current.start('Check disk space')).toBe(true); });
        act(() => {
            deliver(sendMessage.mock.calls[0][3]!, { kind: 'status', status: 'queued', seq: 1,
                main_request_id: 'parent', main_request_session_id: 'root' });
            deliver('parent', { kind: 'turn_started', seq: 2 });
        });
        await waitFor(() => expect(result.current.sessionId).toBe('root'));
        expect(fetchMock.mock.calls.some(([url, options]) =>
            url.includes('/ai-assistant-session?') && options?.cache === 'no-store')).toBe(true);
        act(() => {
            deliver('parent', { kind: 'partial', seq: 3, partial_summary: 'Checking' });
        });
        expect(result.current.partial).toBe('Checking');
        act(() => deliver('parent', { kind: 'permission_required', seq: 4 }));
        act(() => {
            deliver('foreign', { kind: 'turn_started', seq: 0, main_request_id: 'parent', main_request_session_id: 'other' });
            deliver('foreign', { kind: 'partial', seq: 1, partial_summary: 'Foreign' });
            deliver('resume', { kind: 'turn_started', seq: 0, main_request_id: 'parent', main_request_session_id: 'root' });
            deliver('resume', { kind: 'partial', seq: 1, partial_summary: 'Approved command' });
        });
        expect(result.current.partial).toBe('Approved command');
        finished = true;
        await act(async () => deliver('resume', { kind: 'answer', seq: 2, answer: 'Disk report ready' }));
        expect(result.current.messages.at(-1)).toMatchObject({ id: 'saved-report', text: 'Disk report ready' });
    });

    it('recovers a completed approval continuation when only the queued turn start arrived', async () => {
        vi.useFakeTimers();
        let subscriber: SignalingSubscriber | null = null;
        let finished = false;
        const sendMessage = vi.fn((_type: number, _payload: unknown, _to?: string, id?: string) => id!);
        vi.stubGlobal('fetch', vi.fn(async (url: string) => ({
            ok: true, json: async () => {
                const input = sendMessage.mock.calls[0]?.[1] as { client_message_id: string };
                const receipt = { requestId: 'parent', clientMessageId: input.client_message_id,
                    state: 'activated', inputRevision: 1, acceptedAtMs: 1 };
                return { data: url.includes('/requests?') ? { sessionId: 'root', items: [receipt] } : {
                    ...snapshotFields, sessionId: 'root', seq: finished ? 4 : 1, active: false,
                    requestId: finished ? 'permission-resume' : undefined, latestInputSeq: 1,
                    handledInputSeq: finished ? 1 : 0, mainRequests: [receipt],
                    messages: [{ id: input.client_message_id, role: 'user', text: 'Check both devices' },
                        ...(finished ? [{ id: 'answer', role: 'assistant', text: 'Both checks complete' }] : [])],
                } };
            },
        })));
        const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: '', connected: true,
            targetRefs: ['mac', 'linux'], subscribe: handler => { subscriber = handler; return () => { subscriber = null; }; }, sendMessage }));
        const deliver = (id: string, event: Record<string, unknown>) => subscriber?.({
            request_id: id, signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED,
            signaling_data: { request_id: id, ...event },
        });
        try {
            act(() => { expect(result.current.start('Check both devices')).toBe(true); });
            await act(async () => {
                deliver(sendMessage.mock.calls[0][3]!, { kind: 'status', status: 'queued', seq: 1,
                    main_request_id: 'parent', main_request_session_id: 'root' });
                deliver('parent', { kind: 'turn_started', seq: 2 });
            });
            expect(result.current.turnRunning).toBe(true);
            expect(result.current.deliveryState).toBeNull();

            finished = true;
            await act(async () => { await vi.advanceTimersByTimeAsync(2_000); });
            expect(result.current.running).toBe(false);
            expect(result.current.status).toBe('done');
            expect(result.current.messages.at(-1)?.text).toBe('Both checks complete');

            act(() => {
                deliver('permission-resume', { kind: 'turn_started', seq: 0,
                    main_request_id: 'parent', main_request_session_id: 'root' });
                deliver('permission-resume', { kind: 'partial', seq: 1, partial_summary: 'Late progress' });
            });
            expect(result.current.running).toBe(false);
            expect(result.current.partial).toBe('');
            expect(result.current.messages.at(-1)?.text).toBe('Both checks complete');
        } finally { unmount(); }
    });

    it('converges two pages on the same saved timeline during approval and after missed events', async () => {
        vi.useFakeTimers();
        let subscriber: SignalingSubscriber | undefined;
        const sendMessage = vi.fn();
        const receipt = { requestId: 'parent', clientMessageId: 'input', state: 'activated', inputRevision: 1, acceptedAtMs: 1 };
        const permission = { requestId: 'approval', inputRevision: 1, state: 'pending', items: [] };
        const user = { id: 'input', role: 'user', text: 'Check disk space' };
        const proposal = { id: 'proposal', role: 'assistant', text: '', reasoning: 'Ask for permission first.', toolCalls: [
            { id: 'review', name: 'request_permissions', argumentsJson: '{"command":"df -h"}' },
        ] };
        const receiptMessage = { id: 'receipt', role: 'tool', toolCallId: 'review', toolOk: true,
            text: '{"status":"pending_user_decision","request_id":"approval"}' };
        const exec = { id: 'exec', role: 'assistant', text: '', reasoning: 'Run the approved command.', toolCalls: [
            { id: 'disk', name: 'exec_command', argumentsJson: '{"command":"df -h"}' },
        ] };
        const resultMessage = { id: 'output', role: 'tool', toolCallId: 'disk', toolOk: true, text: 'exit_code=0\nFree space: 41 GB' };
        const approved = { ...permission, state: 'approved', decision: { source: 'ai_approval', items: [] } };
        const base = { ...snapshotFields, sessionId: 'root', latestInputSeq: 1, mainRequests: [receipt] };
        let snapshot = { ...base, seq: 1, active: true, requestId: 'parent', handledInputSeq: 0,
            messages: [user], permissionRequests: [] as (typeof approved | typeof permission)[] };
        vi.stubGlobal('fetch', vi.fn(async (url: string) => ({
            ok: true, json: async () => ({ data: url.includes('/requests?') ? { sessionId: 'root', items: [receipt] } : snapshot }),
        })));
        const original = renderHook(() => useAiAssistantChat({ deskId: '', initialConversationId: 'shared-chat',
            subscribe: handler => { subscriber = handler; return () => { subscriber = undefined; }; }, sendMessage }));
        const reopened = renderHook(() => useAiAssistantChat({ deskId: '', initialConversationId: 'shared-chat',
            subscribe: () => () => undefined, sendMessage }));
        const deliver = (id: string, seq: number, event: Record<string, unknown>) => subscriber?.({
            request_id: id, signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED,
            signaling_data: { request_id: id, seq, ...event },
        });
        const poll = () => act(async () => { await vi.advanceTimersByTimeAsync(2_000); });
        const expectSameTimeline = (ids: string[]) => {
            expect(original.result.current.messages.map(message => message.id)).toEqual(ids);
            expect(original.result.current.messages).toEqual(reopened.result.current.messages);
            expect(original.result.current.tools).toEqual(reopened.result.current.tools);
            expect(original.result.current.turnRunning).toBe(reopened.result.current.turnRunning);
        };
        try {
            await poll();
            act(() => deliver('parent', 1, { kind: 'partial', partial_summary: 'Checking' }));
            expect(original.result.current.partial).toBe('Checking');
            await act(async () => deliver('parent', 2, { kind: 'tool_started', tool_name: 'request_permissions', tool_call_id: 'review' }));
            expectSameTimeline(['input']);

            snapshot = { ...snapshot, seq: 2, active: false, handledInputSeq: 1,
                messages: [user, proposal, receiptMessage], permissionRequests: [permission] };
            await poll();
            expectSameTimeline(['input', 'proposal', 'tool-call-review']);
            expect(original.result.current.status).toBe('permission_required');
            expect(original.result.current.tools[0].permissionReview?.state).toBe('pending');

            snapshot = { ...snapshot, seq: 3, active: true, requestId: 'resume',
                messages: [...snapshot.messages, exec], permissionRequests: [approved] };
            await poll();
            expectSameTimeline(['input', 'proposal', 'tool-call-review', 'exec', 'tool-call-disk']);
            expect(original.result.current.tools[0].permissionReview).toEqual({ state: 'approved', source: 'ai_approval' });
            expect(original.result.current.tools[1].output).toBeNull();
            act(() => {
                deliver('parent', 3, { kind: 'turn_started' });
                deliver('resume', 1, { kind: 'partial', partial_summary: 'Summarizing' });
            });
            expect(original.result.current.partial).toBe('Summarizing');

            snapshot = { ...snapshot, seq: 4, messages: [...snapshot.messages, resultMessage] };
            await poll();
            expectSameTimeline(['input', 'proposal', 'tool-call-review', 'exec', 'tool-call-disk', 'output']);
            expect(original.result.current.tools[1]).toMatchObject({ output: resultMessage.text, status: 'ok' });
            expect(original.result.current.turnRunning).toBe(true);

            snapshot = { ...snapshot, seq: 5, active: false, messages: [...snapshot.messages,
                { id: 'answer', role: 'assistant', text: 'Summarizing disk usage.' }] };
            await poll();
            expectSameTimeline(['input', 'proposal', 'tool-call-review', 'exec', 'tool-call-disk', 'output', 'answer']);
            expect(original.result.current.turnRunning).toBe(false);
            expect(original.result.current.partial).toBe('');
            expect(original.result.current.status).toBe('done');
        } finally {
            original.unmount();
            reopened.unmount();
        }
    });

    it('accepts B and C while A runs, and cancels only the selected queued request', async () => {
        let subscriber: SignalingSubscriber | null = null;
        const items: { requestId: string; clientMessageId: string; state: 'queued'; acceptedAtMs: number }[] = [];
        const fetchMock = vi.fn(async (url: string, options?: RequestInit) => {
            if (url.endsWith('/requests/cancel')) {
                const { requestId } = JSON.parse(String(options?.body));
                items.splice(items.findIndex(item => item.requestId === requestId), 1);
                return { ok: true, json: async () => ({ success: true }) };
            }
            return {
                ok: true,
                json: async () => ({ success: true, data: url.includes('/requests?')
                    ? { sessionId: 'original-session', items: [...items] }
                    : { ...snapshotFields, sessionId: 'original-session', seq: 1, active: true,
                        requestId: 'a', messages: [{ id: 'message-a', role: 'user', text: 'A' }] } }),
            };
        });
        vi.stubGlobal('fetch', fetchMock);
        const sendMessage = vi.fn((_type: number, _payload: { client_message_id: string }, _to?: string, id?: string) => id!);
        const { result } = renderHook(() => useAiAssistantChat({
            deskId: 'queue-test',
            initialConversationId: 'queue-conversation',
            subscribe: handler => {
                subscriber = handler;
                return () => { subscriber = null; };
            },
            sendMessage: sendMessage as Parameters<typeof useAiAssistantChat>[0]['sendMessage'],
        }));
        await waitFor(() => expect(result.current.turnRunning).toBe(true));
        for (const [index, question] of ['B', 'C'].entries()) {
            act(() => {
                expect(result.current.start(question)).toBe(true);
                expect(result.current.start('No receipt yet')).toBe(false);
            });
            const [, payload, , requestId] = sendMessage.mock.calls[index];
            items.push({ requestId: question, clientMessageId: payload.client_message_id,
                state: 'queued', acceptedAtMs: index });
            act(() => subscriber?.({
                request_id: requestId!, signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED,
                signaling_data: { request_id: requestId!, seq: 1, kind: 'status', status: 'queued',
                    main_request_id: question, main_request_session_id: 'original-session' },
            }));
            await waitFor(() => expect(result.current.queuedRequests.map(item => item.requestId))
                .toEqual(items.map(item => item.requestId)));
            expect(result.current.deliveryState).toBeNull();
            expect(result.current.turnRunning).toBe(true);
        }
        await act(async () => { await result.current.cancelQueuedRequest('B'); });
        expect(result.current.queuedRequests.map(item => item.requestId)).toEqual(['C']);
        expect(result.current.messages.map(message => message.text)).toEqual(['A']);
        expect(result.current.turnRunning).toBe(true);
    });

    it('continues the stopped queue using the observed control revision', async () => {
        let stopped = true;
        const fetchMock = vi.fn(async (url: string, options?: RequestInit) => {
            if (url.endsWith('/requests/resume')) {
                expect(JSON.parse(String(options?.body))).toEqual({ session: 'original-session', expectedControlRevision: 2 });
                stopped = false;
                return { ok: true, json: async () => ({ success: true, data: { controlRevision: 3 } }) };
            }
            return { ok: true, json: async () => ({ success: true, data: {
                ...snapshotFields, sessionId: 'original-session', seq: stopped ? 5 : 6,
                controlRevision: stopped ? 2 : 3, mainStopped: stopped, active: false, messages: [],
                mainRequests: [{ requestId: 'b', clientMessageId: 'message-b', state: 'queued', acceptedAtMs: 1 }],
            } }) };
        });
        vi.stubGlobal('fetch', fetchMock);
        const sendMessage = vi.fn(() => 'unneeded');
        const { result } = renderHook(() => useAiAssistantChat({ deskId: 'resume-test', initialConversationId: 'resume-conversation',
            sendMessage, subscribe: () => () => undefined }));
        await waitFor(() => expect(result.current.mainStopped).toBe(true));
        await act(async () => { await result.current.resumeQueue(); });
        expect(result.current.mainStopped).toBe(false);
        expect(result.current.queuedRequests.map(item => item.requestId)).toEqual(['b']);
        expect(fetchMock.mock.calls.filter(([url]) => url.endsWith('/requests/resume'))).toHaveLength(1);
        expect(sendMessage).not.toHaveBeenCalled();
    });

    it('hydrates a durable permission request and posts one complete mixed decision', async () => {
        localStorage.setItem('ai-assistant-conversation:desk-1', 'conversation-1');
        let state = 'pending';
        const request = {
            schemaVersion: 1,
            requestId: 'permission-1',
            inputRevision: 3,
            state,
            createdAt: '2026-08-26T00:00:00Z',
            items: [{
                itemId: 'inspect',
                providerId: 'desktop.session',
                toolName: 'inspect_desktop_session',
                expectedEffect: 'read_device',
                resourceScope: ['target:device-1'],
                operationScope: ['observe'],
                exportDestinations: [],
                suggestedTtlSeconds: 300,
                suggestedMaxUses: 1,
                reason: 'Inspect the current target',
            }, {
                itemId: 'inspect-ui',
                providerId: 'desktop.ui',
                toolName: 'inspect_desktop_ui',
                expectedEffect: 'read_device',
                resourceScope: ['target:device-1'],
                operationScope: ['observe-ui'],
                exportDestinations: [],
                suggestedTtlSeconds: 120,
                suggestedMaxUses: 1,
                reason: 'Inspect the current UI tree',
            }],
        };
        const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
            if (String(input).endsWith('/permission-decision')) {
                const body = JSON.parse(String(init?.body));
                expect(body).toEqual({
                    connection: 'desk-1',
                    conversation: 'conversation-1',
                    requestId: 'permission-1',
                    items: [{
                        itemId: 'inspect',
                        decision: 'approve',
                        resource_scope: [],
                        operation_scope: ['observe'],
                        export_destinations: [],
                        ttl_seconds: 60,
                        max_uses: 1,
                    }, {
                        itemId: 'inspect-ui',
                        decision: 'deny',
                    }],
                });
                state = 'partially_approved';
                return { ok: true, json: async () => ({ success: true, data: { state } }) };
            }
            return {
                ok: true,
                json: async () => ({
                    data: {
                        ...snapshotFields,
                        sessionId: 'session-1',
                        seq: state === 'pending' ? 1 : 2,
                        active: false,
                        inputRevision: 3,
                        messages: [{ id: 'user-1', role: 'user', text: 'inspect it' }],
                        permissionRequests: [{ ...request, state }],
                        contextAttachments: [],
                    },
                }),
            };
        });
        vi.stubGlobal('fetch', fetchMock);
        const { result } = renderHook(() => useAiAssistantChat({
            deskId: 'desk-1',
            subscribe: () => () => undefined,
            sendMessage: () => 'request',
        }));

        await waitFor(() => expect(result.current.permissionRequests[0]?.state).toBe('pending'));
        expect(result.current.status).toBe('permission_required');
        await act(async () => {
            expect(await result.current.decidePermissionItems(
                result.current.permissionRequests[0],
                [{
                    itemId: 'inspect',
                    decision: 'approve',
                    resource_scope: [],
                    operation_scope: ['observe'],
                    export_destinations: [],
                    ttl_seconds: 60,
                    max_uses: 1,
                }, {
                    itemId: 'inspect-ui',
                    decision: 'deny',
                }],
            )).toBe(true);
        });
        await waitFor(() => expect(result.current.permissionRequests[0]?.state).toBe('partially_approved'));
        expect(result.current.status).toBe('done');
        expect(result.current.error).toBeNull();
        expect(fetchMock).toHaveBeenCalledWith(
            '/api/my/ai-assistant-session/permission-decision',
            expect.objectContaining({ method: 'POST' }),
        );
    });

    it('shows and revokes a durable capability grant without dispatching a tool', async () => {
        localStorage.setItem('ai-assistant-conversation:desk-1', 'conversation-1');
        let revokedAtUnixMs: number | null = null;
        const grant = {
            grantId: 'grant-1',
            providerId: 'desktop.session',
            capabilityId: 'desktop.session.inspect',
            toolName: 'inspect_desktop_session',
            riskTier: 'r0',
            resourceScope: ['target:device-1'],
            operationScope: ['observe'],
            remainingUses: 1,
            expiresAtUnixMs: Date.now() + 60_000,
            revokedAtUnixMs,
            revokedReason: null,
        };
        const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
            if (String(input).endsWith('/capability-grant/revoke')) {
                expect(JSON.parse(String(init?.body))).toEqual({
                    connection: 'desk-1',
                    conversation: 'conversation-1',
                    grantId: 'grant-1',
                    reason: 'revoked_by_owner',
                });
                revokedAtUnixMs = Date.now();
                return {
                    ok: true,
                    json: async () => ({
                        success: true,
                        data: { ...grant, revokedAtUnixMs, revokedReason: 'revoked_by_owner' },
                    }),
                };
            }
            return {
                ok: true,
                json: async () => ({
                    data: {
                        ...snapshotFields,
                        sessionId: 'session-1',
                        seq: revokedAtUnixMs ? 2 : 1,
                        active: false,
                        messages: [],
                        contextAttachments: [],
                        capabilityGrants: [{
                            ...grant,
                            revokedAtUnixMs,
                            revokedReason: revokedAtUnixMs ? 'revoked_by_owner' : null,
                        }],
                    },
                }),
            };
        });
        vi.stubGlobal('fetch', fetchMock);
        const { result } = renderHook(() => useAiAssistantChat({
            deskId: 'desk-1',
            subscribe: () => () => undefined,
            sendMessage: () => 'request',
        }));

        await waitFor(() => expect(result.current.capabilityGrants).toHaveLength(1));
        await act(async () => {
            expect(await result.current.revokeCapabilityGrant('grant-1')).toBe(true);
        });
        await waitFor(() => expect(
            result.current.capabilityGrants[0]?.revokedReason,
        ).toBe('revoked_by_owner'));
        expect(fetchMock).toHaveBeenCalledWith(
            '/api/my/ai-assistant-session/capability-grant/revoke',
            expect.objectContaining({ method: 'POST' }),
        );
    });

    it('does not require user disposition after an inconclusive action', async () => {
        localStorage.setItem('ai-assistant-conversation:desk-1', 'conversation-1');
        vi.stubGlobal('fetch', vi.fn(async () => ({ ok: true, json: async () => ({ data: {
            ...snapshotFields,
            sessionId: 'session-1', seq: 1, active: false,
            messages: [{ id: 'result-1', role: 'assistant', text: 'Read the current UI before continuing.' }],
            contextAttachments: [],
        } }) })));
        const { result } = renderHook(() => useAiAssistantChat({
            deskId: 'desk-1', subscribe: () => () => undefined, sendMessage: () => 'request',
        }));
        await waitFor(() => expect(result.current.status).toBe('done'));
        expect(result.current.running).toBe(false);
        expect('disposeUnknownOutcome' in result.current).toBe(false);
    });
});

it.each([
    ['file_artifact', 'patch_powerpoint_copy', 'copy.pptx'],
    ['file_artifact', 'replace_word_copy_body', `${'a'.repeat(250)}.DOCX`],
    ['batch_document_artifact', 'patch_keynote_copy', 'copy.key'],
])('restores %s from %s as a visible file result after refresh', async (kind, tool, fileName) => {
    localStorage.setItem('ai-assistant-conversation:copy-device', 'copy-conversation');
    const value = kind === 'file_artifact'
        ? { file_name: fileName, size_bytes: 123, digest_sha256: 'a'.repeat(64) }
        : { file_name: fileName, byte_len: 123, sha256: 'a'.repeat(64), validation_byte_len: 32, validation_sha256: 'b'.repeat(64) };
    const text = JSON.stringify({ result: 'verified', output: { kind, value } });
    vi.stubGlobal('fetch', vi.fn(async () => ({ ok: true, json: async () => ({ data: {
        ...snapshotFields,
        sessionId: 'server-session', seq: 1, active: false,
        messages: [
            { id: 'assistant', role: 'assistant', text: '', toolCalls: [{ id: 'copy-call', name: tool, argumentsJson: '{}' }] },
            { id: 'copy-result', role: 'tool', toolCallId: 'copy-call', text },
        ],
    } }) })));
    const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'copy-device', subscribe: () => () => {}, sendMessage: () => 'request' }));
    try {
        await waitFor(() => expect(result.current.messages.find(message => message.id === 'copy-result')).toMatchObject({ role: 'tool_result', text }));
        expect(result.current.tools[0]).toMatchObject({ callId: 'copy-call', status: 'ok', output: text });
        expect(result.current.messages.filter(message => message.id === 'copy-result')).toHaveLength(1);
    } finally {
        unmount();
        localStorage.removeItem('ai-assistant-conversation:copy-device');
        vi.unstubAllGlobals();
    }
});

it('projects a failed native action with the reason bound to its work record', async () => {
    localStorage.setItem('ai-assistant-conversation:reason-device', 'reason-conversation');
    vi.stubGlobal('fetch', vi.fn(async () => ({ ok: true, json: async () => ({ data: {
        ...snapshotFields,
        sessionId: 'server-session', seq: 1, active: false, actionPermissionReasons: { '44': 'Create the approved calendar event', '45': 'Unrelated reason' },
        messages: [
            { id: 'assistant', role: 'assistant', text: '', toolCalls: [{ id: 'call', name: 'execute_ui_actions', argumentsJson: '{}' }] },
            { id: 'result', role: 'tool', toolCallId: 'call', text: JSON.stringify({ work_id: '44', result: 'definitely_not_started', message: 'target missing' }) },
        ],
    } }) })));
    const { result, unmount } = renderHook(() => useAiAssistantChat({ deskId: 'reason-device', subscribe: () => () => {}, sendMessage: () => 'request' }));
    await waitFor(() => expect(result.current.messages.find(message => message.id === 'result')?.permissionReason).toBe('Create the approved calendar event'));
    expect(result.current.tools[0]).toMatchObject({ status: 'failed', permissionReason: 'Create the approved calendar event' });
    unmount();
    localStorage.removeItem('ai-assistant-conversation:reason-device');
    vi.unstubAllGlobals();
});

it('distinguishes returned historical results from explicit tool errors', async () => {
    localStorage.setItem('ai-assistant-conversation:tool-status-device', 'tool-status-conversation');
    const calls = [
        { id: 'read', name: 'inspect_desktop_ui', text: '{"ReadContext":{"DesktopUiInspect":{}}}' },
        { id: 'permission', name: 'request_permissions', text: '{"status":"pending_user_decision","request_id":"request"}' },
        { id: 'confirmed', name: 'inspect_desktop_ui', text: '{"ReadContext":{}}', toolOk: true },
        { id: 'rejected', name: 'inspect_desktop_ui', text: '{"message":"request rejected"}', toolOk: false },
        { id: 'error', name: 'inspect_desktop_ui', text: '{"status":"error","message":"device unavailable"}' },
        { id: 'wait-error', name: 'wait_for_command', text: 'wait error: device disconnected' },
    ];
    vi.stubGlobal('fetch', vi.fn(async () => ({ ok: true, json: async () => ({ data: {
        ...snapshotFields,
        sessionId: 'server-session', seq: 1, active: false,
        messages: [
            { id: 'assistant', role: 'assistant', text: '', toolCalls: calls.map(call => ({ id: call.id, name: call.name, argumentsJson: '{}' })) },
            ...calls.map(call => ({ id: `result-${call.id}`, role: 'tool', toolCallId: call.id, text: call.text,
                toolOk: 'toolOk' in call ? call.toolOk : undefined })),
        ],
    } }) })));
    const { result, unmount } = renderHook(() => useAiAssistantChat({
        deskId: 'tool-status-device', subscribe: () => () => {}, sendMessage: () => 'request',
    }));
    try {
        await waitFor(() => expect(result.current.tools).toHaveLength(calls.length));
        expect(Object.fromEntries(result.current.tools.map(tool => [tool.callId, tool.status]))).toEqual({
            read: 'returned', permission: 'returned', confirmed: 'ok', rejected: 'failed',
            error: 'failed', 'wait-error': 'failed',
        });
    } finally {
        unmount();
        localStorage.removeItem('ai-assistant-conversation:tool-status-device');
        vi.unstubAllGlobals();
    }
});

it('upgrades a returned tool result when a newer snapshot carries its durable outcome', async () => {
    vi.useFakeTimers();
    localStorage.setItem('ai-assistant-conversation:outcome-refresh-device', 'outcome-refresh-conversation');
    let toolOk: boolean | undefined;
    let seq = 1;
    vi.stubGlobal('fetch', vi.fn(async () => ({ ok: true, json: async () => ({ data: {
        ...snapshotFields,
        sessionId: 'server-session', seq, active: false,
        messages: [
            { id: 'assistant', role: 'assistant', text: '', toolCalls: [{ id: 'call', name: 'inspect_desktop_ui', argumentsJson: '{}' }] },
            { id: 'result', role: 'tool', toolCallId: 'call', text: '{"ReadContext":{}}', toolOk },
        ],
    } }) })));
    const { result, unmount } = renderHook(() => useAiAssistantChat({
        deskId: 'outcome-refresh-device', subscribe: () => () => {}, sendMessage: () => 'request',
    }));
    try {
        await act(async () => { await Promise.resolve(); });
        expect(result.current.tools[0]?.status).toBe('returned');
        seq = 2;
        toolOk = true;
        await act(async () => { await vi.advanceTimersByTimeAsync(2_000); });
        expect(result.current.tools[0]?.status).toBe('ok');
    } finally {
        unmount();
        localStorage.removeItem('ai-assistant-conversation:outcome-refresh-device');
        vi.unstubAllGlobals();
        vi.useRealTimers();
    }
});
