import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { MemoryRouter, useLocation } from 'react-router-dom';
import SchedulePage from './page';
import type { SendTrackedOptions, SignalingSubscriber } from '@/features/desk/use-desk-signaling';
import { deskErrorCodeEnum } from '@/services/types';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
const transport = vi.hoisted(() => ({ subscriber: (() => {}) as SignalingSubscriber, send: vi.fn(), cancel: vi.fn() }));
vi.mock('@/features/desk/use-desk-signaling', () => ({ useDeskSignaling: () => ({ isConnected: true, subscribe, sendTracked: transport.send, cancelQueued: transport.cancel }) }));
function subscribe(callback: SignalingSubscriber) { transport.subscriber = callback; return () => {}; }
const task = { schedule_id: 'task-1', target_device_id: 'device-1', kind: 'fresh_task', title: 'Original', prompt: 'Report status', status: 'draft', revision: 1, spec: { schema_version: 1, rule: { kind: 'daily', utc_time: '06:00:00' } }, next_run_at: null, active_run_id: null, pause_reasons: [], upcoming_runs: [], consecutive_failures: 0, failure_threshold: 3, created_at: '2026-09-06T00:00:00Z', updated_at: '2026-09-06T00:00:00Z' };
let storedTasks = [task];
function respond(request: SendTrackedOptions, data: unknown) {
    const mutation = data as { result?: string; task?: typeof task };
    if (mutation.result === 'task' && mutation.task) storedTasks = [mutation.task, ...storedTasks.filter(item => item.schedule_id !== mutation.task!.schedule_id)];
    if (request.data.operation === 'search') {
        const response = data as { tasks: typeof task[] };
        const tasks = response.tasks.filter(item => item.kind === request.data.kind);
        data = { result: 'search_results', tasks, next_cursor: null, total: tasks.length, attention_count: 0 };
    }
    transport.subscriber({ signaling_type: 646, request_id: request.requestId, response_state: { error_code: deskErrorCodeEnum.SUCCESS }, signaling_data: data });
}
function operations() { return transport.send.mock.calls.map(([r]) => r.data.operation); }
beforeEach(() => {
    transport.send.mockReset();
    storedTasks = [task];
    transport.send.mockImplementation((request: SendTrackedOptions) => {
        if (request.data.operation === 'search') queueMicrotask(() => respond(request, { result: 'search_results', tasks: storedTasks, next_cursor: null }));
        return { requestId: request.requestId!, disposition: 'sent' };
    });
});
afterEach(() => vi.unstubAllGlobals());
function Location() { return <output data-testid="location">{useLocation().search}</output>; }

describe('scheduled task management page', () => {
    it('closes an unchanged time edit without conversion or mutation', async () => {
        render(<MemoryRouter><SchedulePage devices={[]} /></MemoryRouter>);
        await screen.findByText('Original');
        fireEvent.click(screen.getByRole('button', { name: 'Change time' }));
        const dialog = screen.getByRole('dialog');
        fireEvent.click(within(dialog).getByRole('button', { name: 'Save', exact: true }));
        expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
        expect(operations()).toEqual(['search']);
    });
    it.each(['fresh_task', 'conversation_resume'])('keeps history, rename and delete but hides rescheduling for completed %s', async kind => {
        transport.send.mockImplementation((request: SendTrackedOptions) => {
            if (request.data.operation === 'search') queueMicrotask(() => respond(request, { result: 'search_results', tasks: [{ ...task, kind, status: 'completed' }], next_cursor: null }));
            return { requestId: request.requestId!, disposition: 'sent' };
        });
        render(<MemoryRouter><SchedulePage devices={[]} /></MemoryRouter>);
        if (kind === 'conversation_resume') fireEvent.click(screen.getByRole('tab', { name: 'Conversation continuations' }));
        await screen.findByText('Original');
        expect(screen.queryByRole('button', { name: 'Change time' })).not.toBeInTheDocument();
        expect(screen.getByRole('button', { name: 'Rename' })).toBeEnabled();
        expect(screen.getByRole('button', { name: 'Delete' })).toBeEnabled();
        expect(screen.getByRole('button', { name: 'Run history' })).toBeEnabled();
        expect(operations()).toEqual(kind === 'conversation_resume' ? ['search', 'search'] : ['search']);
    });

    it('creates a same-conversation draft and enables it only after explicit confirmation', async () => {
        storedTasks = [];
        transport.send.mockImplementation((request: SendTrackedOptions) => {
            if (request.data.operation === 'search') queueMicrotask(() => respond(request, { result: 'search_results', tasks: storedTasks, next_cursor: null }));
            if (request.data.operation === 'convert_time') queueMicrotask(() => respond(request, { result: 'converted_time', upcoming_runs: ['2027-01-01T22:00:00Z'], conversion: { conversion_version: '1', offset_seconds: -28800, spec: { schema_version: 1, rule: { kind: 'once', at: '2027-01-01T22:00:00Z' } } } }));
            if (request.data.operation === 'create_draft') queueMicrotask(() => respond(request, { result: 'task', task: { ...task, kind: 'conversation_resume', title: 'Continue report', revision: 3 } }));
            if (request.data.operation === 'activate_conversation_resume') queueMicrotask(() => respond(request, { result: 'task', task: { ...task, kind: 'conversation_resume', title: 'Continue report', status: 'active', revision: 4 } }));
            return { requestId: request.requestId!, disposition: 'sent' };
        });
        render(<MemoryRouter initialEntries={['/schedules?resume_conversation=chat-1&resume_device=device-1&resume_revision=7']}><SchedulePage devices={[{ id: 'device-1', name: 'Office PC' }]} /></MemoryRouter>);
        await waitFor(() => expect(screen.getByRole('button', { name: 'Schedule this conversation' })).toBeEnabled());
        fireEvent.click(screen.getByRole('button', { name: 'Schedule this conversation' }));
        expect(within(screen.getByRole('dialog')).getByLabelText('Device')).toBeDisabled();
        fireEvent.change(within(screen.getByRole('dialog')).getByLabelText('Task name'), { target: { value: 'Continue report' } });
        fireEvent.change(screen.getByLabelText('What should this task do?'), { target: { value: 'Continue the report' } });
        fireEvent.change(screen.getByLabelText('Reference / start date'), { target: { value: '2027-01-01' } });
        fireEvent.change(screen.getByLabelText('Time'), { target: { value: '14:00' } });
        await waitFor(() => expect(screen.getByRole('button', { name: 'Preview time' })).toBeEnabled());
        fireEvent.click(screen.getByRole('button', { name: 'Preview time' }));
        fireEvent.click(await screen.findByRole('button', { name: 'Confirm time and save' }));
        await screen.findByRole('button', { name: 'Enable scheduled continuation' });
        expect(operations().filter(operation => operation !== 'search')).toEqual(['convert_time', 'convert_time', 'create_draft']);
        expect(transport.send.mock.calls.find(([request]) => request.data.operation === 'create_draft')![0].data.draft).toMatchObject({ kind: 'conversation_resume', target_device_id: 'device-1', source_conversation_id: 'chat-1', requirement_revision: 7, spec: { rule: { kind: 'once' } } });
        fireEvent.click(screen.getByRole('button', { name: 'Enable scheduled continuation' }));
        await waitFor(() => expect(operations().filter(operation => operation !== 'search')).toEqual(['convert_time', 'convert_time', 'create_draft', 'activate_conversation_resume']));
        expect(transport.send.mock.calls.find(([request]) => request.data.operation === 'activate_conversation_resume')![0].data).toEqual({ operation: 'activate_conversation_resume', schedule_id: 'task-1', expected_revision: 3 });
        await waitFor(() => expect(screen.queryByRole('button', { name: 'Enable scheduled continuation' })).not.toBeInTheDocument());
    });

    it('opens an occurrence deep link even when its task is not on the first list page', async () => {
        const fetch = vi.fn().mockResolvedValue({ ok: true, json: async () => ({ code: deskErrorCodeEnum.SUCCESS, data: { sessionId: 'private-session', seq: 1, messages: [{ id: 'answer', role: 'assistant', text: 'Bookmarked result' }], messagePage: { hasMore: false } } }) });
        vi.stubGlobal('fetch', fetch);
        render(<MemoryRouter initialEntries={['/schedules?scheduled_task=off-page&scheduled_run=run-2']}><Location /><SchedulePage devices={[]} /></MemoryRouter>);
        expect(await screen.findByText('Bookmarked result')).toBeInTheDocument();
        const url = new URL(fetch.mock.calls[0][0], 'https://example.invalid');
        expect(url.searchParams.get('scheduled_task')).toBe('off-page');
        expect(url.searchParams.get('scheduled_run')).toBe('run-2');
        expect(screen.getByTestId('location')).not.toHaveTextContent('private-session');
        fireEvent.click(screen.getByText('Back to run history'));
        expect(screen.getByTestId('location')).toHaveTextContent('scheduled_task=off-page');
        expect(screen.getByTestId('location')).not.toHaveTextContent('scheduled_run');
        expect(operations().every(operation => operation === 'search' || operation === 'list_runs')).toBe(true);
    });
    it('routes an off-page bookmarked approval through its current device connection', async () => {
        transport.send.mockImplementation((request: SendTrackedOptions) => {
            if (request.data.operation === 'search') queueMicrotask(() => respond(request, { result: 'search_results', tasks: [], next_cursor: null }));
            if (request.data.operation === 'list_runs') queueMicrotask(() => respond(request, { result: 'runs', schedule_id: 'off-page', runs: [], next_cursor: null }));
            if (request.data.operation === 'get') queueMicrotask(() => respond(request, { result: 'task', task: { ...task, schedule_id: 'off-page', active_run_id: 'run-2', status: 'triggered' } }));
            return { requestId: request.requestId!, disposition: 'sent' };
        });
        const pending = { schemaVersion: 1, requestId: 'permission', inputRevision: 1, state: 'pending', createdAt: '2026-09-06T00:00:00Z', items: [{
            itemId: 'read', providerId: 'desktop.session', toolName: 'inspect_desktop_session', expectedEffect: 'read_device', reason: 'Review bookmarked run',
            resourceScope: ['target:device-1'], operationScope: ['observe'], exportDestinations: [], suggestedTtlSeconds: 60, suggestedMaxUses: 1,
        }] };
        const saved = (state: string) => ({ ok: true, json: async () => ({ code: deskErrorCodeEnum.SUCCESS, data: {
            sessionId: 'original-session', seq: state === 'pending' ? 1 : 2, inputRevision: 1, requestId: 'run-2',
            messages: [], messagePage: { hasMore: false }, permissionRequests: [{ ...pending, state }],
        } }) });
        const fetch = vi.fn().mockResolvedValueOnce(saved('pending'))
            .mockResolvedValueOnce({ ok: true, json: async () => ({ code: deskErrorCodeEnum.SUCCESS, data: { state: 'denied' } }) })
            .mockResolvedValueOnce(saved('denied'));
        vi.stubGlobal('fetch', fetch);
        render(<MemoryRouter initialEntries={['/schedules?scheduled_task=off-page&scheduled_run=run-2']}>
            <SchedulePage devices={[{ id: 'device-1', name: 'Online device', connectionId: 'live-host' }]} />
        </MemoryRouter>);
        await waitFor(() => expect(screen.getByRole('button', { name: 'Submit selected permissions' })).toBeEnabled());
        fireEvent.click(screen.getByRole('button', { name: 'Deny' }));
        await waitFor(() => expect(fetch).toHaveBeenCalledTimes(3));
        expect(JSON.parse(fetch.mock.calls[1][1].body)).toMatchObject({ connection: 'live-host', session: 'original-session', expectedRunRequestId: 'run-2',
            items: [{ itemId: 'read', decision: 'deny' }] });
        await waitFor(() => expect(screen.queryByRole('button', { name: 'Submit selected permissions' })).not.toBeInTheDocument());
    });
    it('rejects an incomplete deep link without requesting a conversation', async () => {
        const fetch = vi.fn(); vi.stubGlobal('fetch', fetch);
        render(<MemoryRouter initialEntries={['/schedules?scheduled_run=run-2']}><SchedulePage devices={[]} /></MemoryRouter>);
        expect(await screen.findByRole('alert')).toHaveTextContent('Invalid task or run link.');
        expect(fetch).not.toHaveBeenCalled();
        expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    });

    it('opens guided run history by task without starting or publishing anything', async () => {
        render(<MemoryRouter><SchedulePage devices={[{ id: 'device-1', name: 'Office PC' }]} /></MemoryRouter>);
        await screen.findByText('Original');
        fireEvent.click(screen.getByRole('button', { name: 'Guided run and permissions' }));
        await waitFor(() => expect(operations()).toEqual(['search', 'get_task_rehearsal']));
        const request = transport.send.mock.calls[1][0];
        expect(request.data.schedule_id).toBe('task-1');
        await act(async () => respond(request, { result: 'task_rehearsal', task, rehearsal: null }));
        expect(within(screen.getByRole('dialog')).getByText('No guided run has been recorded for this task.')).toBeInTheDocument();
        expect(operations()).toEqual(['search', 'get_task_rehearsal']);
    });

    it('shows a rejected edit inside the dialog and retries the same request', async () => {
        render(<MemoryRouter><SchedulePage devices={[{ id: 'device-1', name: 'Office PC' }]} /></MemoryRouter>);
        await screen.findByText('Original');
        expect(screen.getByRole('button', { name: 'Change time' })).toBeEnabled();
        fireEvent.click(screen.getByRole('button', { name: 'Rename' }));
        fireEvent.change(within(screen.getByRole('dialog')).getByLabelText('Task name'), { target: { value: 'New title' } });
        fireEvent.click(screen.getByRole('button', { name: 'Save' }));
        await waitFor(() => expect(operations()).toEqual(['search', 'rename']));
        const request = transport.send.mock.calls[1][0];
        await act(async () => transport.subscriber({ signaling_type: 646, request_id: request.requestId, response_state: { error_code: deskErrorCodeEnum.PRECONDITION_FAILED, message: 'Task changed; refresh before editing.' } }));
        expect(within(screen.getByRole('dialog')).getByRole('alert')).toHaveTextContent('Task changed; refresh before editing.');
        expect(within(screen.getByRole('dialog')).getByLabelText('Task name')).toBeDisabled();
        fireEvent.click(screen.getByRole('button', { name: 'Save' }));
        await waitFor(() => expect(operations()).toEqual(['search', 'rename', 'rename']));
        expect(transport.send.mock.calls[2][0].data).toEqual(request.data);
        await act(async () => respond(transport.send.mock.calls[2][0], { result: 'task', task: { ...task, title: 'New title', revision: 2 } }));
        expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    });
    it('changing display timezone and switching tabs never changes the persisted schedule', async () => {
        render(<MemoryRouter><SchedulePage devices={[{ id: 'device-1', name: 'Office PC' }]} /></MemoryRouter>);
        await screen.findByText('Original');
        fireEvent.change(screen.getByLabelText('Display and input time zone'), { target: { value: 'Asia/Shanghai' } });
        fireEvent.click(screen.getByRole('tab', { name: 'Conversation continuations' }));
        expect(screen.queryByText('Original')).not.toBeInTheDocument();
        expect(operations()).toEqual(['search', 'search']);
    });
    it('renames without converting or resaving the time rule', async () => {
        render(<MemoryRouter><SchedulePage devices={[{ id: 'device-1', name: 'Office PC' }]} /></MemoryRouter>);
        await screen.findByText('Original');
        expect(screen.getByRole('button', { name: 'Change time' })).toBeEnabled();
        fireEvent.click(screen.getByRole('button', { name: 'Rename' }));
        fireEvent.change(within(screen.getByRole('dialog')).getByLabelText('Task name'), { target: { value: 'New title' } });
        fireEvent.click(screen.getByRole('button', { name: 'Save' }));
        await waitFor(() => expect(operations()).toEqual(['search', 'rename']));
        const request = transport.send.mock.calls[1][0];
        expect(request.data).toEqual({ operation: 'rename', schedule_id: 'task-1', expected_revision: 1, title: 'New title' });
        await act(async () => respond(request, { result: 'task', task: { ...task, title: 'New title', revision: 2 } }));
        expect(await screen.findByText('New title')).toBeInTheDocument();
    });
    it('closing an editor during time conversion prevents a late draft creation', async () => {
        render(<MemoryRouter><SchedulePage devices={[{ id: 'device-1', name: 'Office PC' }]} /></MemoryRouter>);
        await screen.findByText('Original');
        fireEvent.click(screen.getByRole('button', { name: 'New automation draft' }));
        fireEvent.change(within(screen.getByRole('dialog')).getByLabelText('Task name'), { target: { value: 'New draft' } });
        fireEvent.change(screen.getByLabelText('What should this task do?'), { target: { value: 'Read status' } });
        fireEvent.change(screen.getByLabelText('Reference / start date'), { target: { value: '2026-09-07' } });
        fireEvent.change(screen.getByLabelText('Time'), { target: { value: '14:00' } });
        fireEvent.click(screen.getByRole('button', { name: 'Preview time' }));
        await waitFor(() => expect(operations()).toEqual(['search', 'convert_time']));
        const request = transport.send.mock.calls[1][0];
        fireEvent.keyDown(screen.getByRole('dialog'), { key: 'Escape', code: 'Escape' });
        await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
        await act(async () => respond(request, { result: 'converted_time', upcoming_runs: ['2027-01-01T22:00:00Z'], conversion: { conversion_version: '1', offset_seconds: -28800, spec: { schema_version: 1, rule: { kind: 'daily', utc_time: '06:00:00' } } } }));
        expect(operations()).toEqual(['search', 'convert_time']);
    });

});
