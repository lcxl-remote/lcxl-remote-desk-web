import { act, fireEvent, render, renderHook, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { AiAssistantSubAgentSummary } from '@/services/types';
import { deskErrorCodeEnum } from '@/services/types';
import { useAiAssistantChat } from './use-ai-assistant-chat';
import { AssistantStopConfirmation } from './assistant-stop-confirmation';
import { SIGNALING_TYPE_CODE_AI_ASSISTANT_SESSION_SELECTED } from './constants';
import type { SignalingSubscriber } from './use-desk-signaling';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
const child: AiAssistantSubAgentSummary = { task_id: 'task-1', child_session_id: 'child-session', group_id: 'group-1',
    name: 'Research', state: 'running', input_revision: 1, control_revision: 1, state_revision: 2,
    source_goal_id: null, source: { kind: 'user_input', input_revision: 1 }, wait_reason: null,
    created_at: '2026-09-30T00:00:00Z', updated_at: '2026-09-30T00:00:01Z' };
const root = (children: AiAssistantSubAgentSummary[] = []) => ({ sessionId: 'main-session', seq: 10,
    inputRevision: 1, controlRevision: 1, mainStopped: false, active: true, requestId: 'main-request',
    messages: [{ id: 'owner-input', role: 'user', text: 'Keep this request' }],
    subagents: { parent_session_id: null, task: null, active_tasks: children, attention_tasks: [], attention_count: 0,
        tasks: { items: children, total: children.length, unfinished: children.length, has_more: false, next_cursor: null } } });
const ok = (data: unknown) => ({ ok: true, json: async () => ({ success: true, code: deskErrorCodeEnum.SUCCESS, data }) });
function mount(connected = true) {
    localStorage.setItem('ai-assistant-conversation:stop-device', 'owner-conversation');
    const sendMessage = vi.fn().mockReturnValue('new-request');
    let subscriber: SignalingSubscriber | undefined;
    const hook = renderHook(() => useAiAssistantChat({ deskId: 'stop-device', connected,
        subscribe: handler => { subscriber = handler; return () => { subscriber = undefined; }; }, sendMessage }));
    if (connected) {
        act(() => subscriber?.({ request_id: 'new-request', signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_SESSION_SELECTED,
            signaling_data: { target: null }, response_state: { error_code: deskErrorCodeEnum.SUCCESS } }));
        sendMessage.mockClear();
    }
    return { ...hook, sendMessage };
}

describe('durable owner stop', () => {
    beforeEach(() => localStorage.clear());
    afterEach(() => vi.unstubAllGlobals());

    it('submits one fenced control and retains the transcript after a committed stop', async () => {
        let snapshot = root();
        let settle!: (value: ReturnType<typeof ok>) => void;
        const fetch = vi.fn((url: string, _init?: RequestInit) => url.endsWith('/stop')
            ? new Promise<ReturnType<typeof ok>>(resolve => { settle = resolve; }) : Promise.resolve(ok(snapshot)));
        vi.stubGlobal('fetch', fetch);
        const { result, sendMessage, unmount } = mount();
        await waitFor(() => expect(result.current.canStop).toBe(true));
        act(() => { result.current.stop(); result.current.stop(); });
        const controls = fetch.mock.calls.filter(([url]) => url.endsWith('/stop'));
        expect(controls).toHaveLength(1);
        expect(JSON.parse((controls[0][1] as RequestInit).body as string)).toMatchObject({
            connection: 'stop-device', conversation: 'owner-conversation', session: 'main-session',
            control: { expected_input_revision: 1, expected_control_revision: 1, subagent_choice: null },
        });
        expect(result.current.stopping).toBe(true);
        snapshot = { ...snapshot, seq: 11, active: false, mainStopped: true, controlRevision: 2 };
        await act(async () => { settle(ok({ input_revision: 1, control_revision: 2, stopped_subagents: [] })); });
        await waitFor(() => expect(result.current.stopping).toBe(false));
        expect(result.current.messages[0].text).toBe('Keep this request');
        expect(result.current.turnRunning).toBe(false);
        expect(sendMessage).not.toHaveBeenCalled();
        unmount();
    });

    it('opens confirmation without changing child state; dismiss is a no-op', async () => {
        const fetch = vi.fn().mockResolvedValue(ok(root([child]))); vi.stubGlobal('fetch', fetch);
        const { result, unmount } = mount();
        await waitFor(() => expect(result.current.subagents.active_tasks).toHaveLength(1));
        act(() => result.current.stop());
        expect(result.current.stopConfirmation).not.toBeNull();
        expect(fetch.mock.calls.some(([url]) => String(url).endsWith('/stop'))).toBe(false);
        act(() => result.current.dismissStopConfirmation());
        expect(result.current.stopConfirmation).toBeNull();
        expect(result.current.subagents.active_tasks[0].state).toBe('running');
        unmount();
    });

    it('can stop only the main assistant while an existing child remains active', async () => {
        let snapshot = root([child]);
        const fetch = vi.fn(async (url: string, init?: RequestInit) => {
            if (url.endsWith('/stop')) {
                expect(JSON.parse(init!.body as string).control.subagent_choice).toBe('main_only');
                snapshot = { ...snapshot, seq: 11, controlRevision: 2, mainStopped: true, active: false };
                return ok({ input_revision: 1, control_revision: 2, stopped_subagents: [] });
            }
            return ok(snapshot);
        }); vi.stubGlobal('fetch', fetch);
        const { result, unmount } = mount();
        await waitFor(() => expect(result.current.canStop).toBe(true));
        act(() => result.current.stop());
        await act(async () => { expect(await result.current.confirmStop(false)).toBe(true); });
        expect(result.current.mainStopped).toBe(true);
        expect(result.current.subagents.active_tasks[0].state).toBe('running');
        expect(result.current.canStop).toBe(true);
        expect(result.current.turnRunning).toBe(false);
        unmount();
    });

    it('does not automatically replay a stop rejected after a child appeared', async () => {
        let snapshot = root();
        const fetch = vi.fn(async (url: string) => {
            if (url.endsWith('/stop')) {
                snapshot = root([child]); snapshot.seq = 11;
                return { ok: true, json: async () => ({ code: deskErrorCodeEnum.PRECONDITION_FAILED, message: 'Refresh confirmation' }) };
            }
            return ok(snapshot);
        }); vi.stubGlobal('fetch', fetch);
        const { result, unmount } = mount();
        await waitFor(() => expect(result.current.canStop).toBe(true));
        act(() => result.current.stop());
        await waitFor(() => expect(result.current.error).toBe('Refresh confirmation'));
        expect(fetch.mock.calls.filter(([url]) => url.endsWith('/stop'))).toHaveLength(1);
        act(() => result.current.stop());
        expect(result.current.stopConfirmation).not.toBeNull();
        expect(fetch.mock.calls.filter(([url]) => url.endsWith('/stop'))).toHaveLength(1);
        unmount();
    });

    it('a new owner input closes the old confirmation without cancelling its child', async () => {
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue(ok(root([child]))));
        const { result, sendMessage, unmount } = mount();
        await waitFor(() => expect(result.current.canStop).toBe(true));
        act(() => result.current.stop());
        act(() => { result.current.start('A new request'); });
        expect(result.current.stopConfirmation).toBeNull();
        expect(result.current.subagents.active_tasks[0]).toEqual(child);
        expect(sendMessage).toHaveBeenCalledTimes(1);
        unmount();
    });

    it('offers durable stop for idle children while the original device is offline', async () => {
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue(ok({ ...root([child]), active: false, mainStopped: true })));
        const { result, unmount } = mount(false);
        await waitFor(() => expect(result.current.canStop).toBe(true));
        act(() => result.current.stop());
        expect(result.current.stopConfirmation?.session).toBe('main-session');
        unmount();
    });
});

describe('stop confirmation', () => {
    it('defaults to include subtasks; dismissing never submits a stop', async () => {
        const onDismiss = vi.fn(); const onConfirm = vi.fn().mockResolvedValue(true);
        const view = render(<AssistantStopConfirmation open busy={false} onDismiss={onDismiss} onConfirm={onConfirm} />);
        expect(view.getByRole('checkbox')).toHaveAttribute('data-state', 'checked');
        fireEvent.click(view.getByRole('button', { name: 'Stop', exact: true }));
        expect(onConfirm).toHaveBeenCalledWith(true);
        fireEvent.click(view.getByRole('button', { name: 'Keep running', exact: true }));
        expect(onDismiss).toHaveBeenCalledTimes(1);
        expect(onConfirm).toHaveBeenCalledTimes(1);
        view.unmount();
    });
});
