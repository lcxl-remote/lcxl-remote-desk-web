import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { RunPermissions } from './run-permissions';
import type { ScheduleManagementResponse, ScheduleView } from '@/services/types';
import { deskErrorCodeEnum } from '@/services/types';
vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
type Props = Parameters<typeof RunPermissions>[0];
const snapshot: Props['snapshot'] = {
    sessionId: 'original-session', seq: 10, inputRevision: 3, requestId: 'run-1',
    permissionRequests: [{ schemaVersion: 1, requestId: 'permission-1', inputRevision: 3, state: 'pending', createdAt: '2026-09-06T00:00:00Z',
        items: [{ itemId: 'read', providerId: 'desktop.session', toolName: 'inspect_desktop_session', expectedEffect: 'read_device',
            reason: 'Read selected device', resourceScope: ['target:device-1'], operationScope: ['observe'], exportDestinations: [],
            suggestedMaxUses: 1, suggestedTtlSeconds: 60 }] }],
};
const task = { schedule_id: 'task-1', target_device_id: 'device-1', active_run_id: 'run-1', status: 'triggered' } as ScheduleView;
function props(): Props {
    return { scheduleId: 'task-1', runId: 'run-1', snapshot, connected: true, loading: false,
        connectionIds: { 'device-1': 'current-connection' }, onReload: vi.fn().mockResolvedValue(undefined),
        client: { request: vi.fn().mockResolvedValue({ result: 'task', task }) } };
}
const result = { ok: true, json: async () => ({ code: deskErrorCodeEnum.SUCCESS, data: { state: 'approved' } }) };
const submit = () => screen.getByRole('button', { name: 'Submit selected permissions' });
afterEach(() => vi.unstubAllGlobals());
describe('scheduled run permission submission', () => {
    it('binds a reviewed decision to the original session and selected run then reloads', async () => {
        const input = props(); const fetch = vi.fn().mockResolvedValue(result); vi.stubGlobal('fetch', fetch);
        render(<RunPermissions {...input} />);
        await waitFor(() => expect(submit()).toBeEnabled());
        fireEvent.click(submit());
        await waitFor(() => expect(input.onReload).toHaveBeenCalledTimes(1));
        const [url, options] = fetch.mock.calls[0];
        expect(url).toBe('/api/my/device-assistant-session/permission-decision');
        expect(options.credentials).toBe('include');
        expect(JSON.parse(options.body)).toEqual({ connection: 'current-connection', session: 'original-session', requestId: 'permission-1',
            expectedRunRequestId: 'run-1', items: [{ itemId: 'read', decision: 'approve', resource_scope: ['target:device-1'],
                operation_scope: ['observe'], export_destinations: [], ttl_seconds: 60, max_uses: 1 }] });
        expect(await screen.findByText('Decision recorded. The conversation has been refreshed to check execution progress.')).toBeInTheDocument();
    });
    it.each(['other-run', 'other-input', 'offline', 'wrong-device', 'other-active-run', 'deleted', 'wrong-task', 'metadata-failed', 'prototype-name'])(
        'does not submit when %s', async reason => {
            const input = props();
            if (reason === 'other-run') input.snapshot = { ...snapshot, requestId: 'another-run' };
            if (reason === 'other-input') input.snapshot = { ...snapshot, inputRevision: 4 };
            if (reason === 'offline') input.connectionIds = {};
            if (reason === 'wrong-device') input.connectionIds = { 'another-device': 'other-connection' };
            if (reason === 'other-active-run') input.client.request = vi.fn().mockResolvedValue({ result: 'task', task: { ...task, active_run_id: 'another-run' } });
            if (reason === 'deleted') input.client.request = vi.fn().mockResolvedValue({ result: 'task', task: { ...task, status: 'deleted' } });
            if (reason === 'wrong-task') input.client.request = vi.fn().mockResolvedValue({ result: 'task', task: { ...task, schedule_id: 'another-task' } });
            if (reason === 'metadata-failed') input.client.request = vi.fn().mockRejectedValue(new Error('metadata unavailable'));
            if (reason === 'prototype-name') {
                input.connectionIds = {};
                input.client.request = vi.fn().mockResolvedValue({ result: 'task', task: { ...task, target_device_id: 'toString' } });
            }
            const fetch = vi.fn(); vi.stubGlobal('fetch', fetch);
            render(<RunPermissions {...input} />);
            await act(async () => {});
            const button = screen.queryByRole('button', { name: 'Submit selected permissions' });
            if (button) { expect(button).toBeDisabled(); fireEvent.click(button); }
            expect(fetch).not.toHaveBeenCalled();
        },
    );
    it('does not retry an uncertain submission and reloads its durable state', async () => {
        const input = props(); const fetch = vi.fn().mockRejectedValue(new Error('lost response')); vi.stubGlobal('fetch', fetch);
        render(<RunPermissions {...input} />);
        await waitFor(() => expect(submit()).toBeEnabled()); fireEvent.click(submit());
        await waitFor(() => expect(input.onReload).toHaveBeenCalledTimes(1));
        expect(fetch).toHaveBeenCalledTimes(1);
        expect(await screen.findByRole('alert')).toHaveTextContent('The decision could not be confirmed.');
    });
    it('prevents double submission and ignores a response after disconnect', async () => {
        let resolve!: (value: unknown) => void;
        const fetch = vi.fn(() => new Promise(done => { resolve = done; })); vi.stubGlobal('fetch', fetch);
        const input = props(); const view = render(<RunPermissions {...input} />);
        await waitFor(() => expect(submit()).toBeEnabled());
        fireEvent.click(submit()); fireEvent.click(submit());
        expect(fetch).toHaveBeenCalledTimes(1);
        view.rerender(<RunPermissions {...input} connected={false} />);
        await act(async () => resolve(result));
        expect(input.onReload).not.toHaveBeenCalled();
        expect(screen.queryByText('Decision recorded. The conversation has been refreshed to check execution progress.')).not.toBeInTheDocument();
    });
    it('requires metadata for the refreshed snapshot before accepting another decision', async () => {
        const input = props();
        let resolve!: (value: ScheduleManagementResponse) => void;
        input.client.request = vi.fn().mockResolvedValueOnce({ result: 'task', task })
            .mockImplementationOnce(() => new Promise<ScheduleManagementResponse>(done => { resolve = done; }));
        const view = render(<RunPermissions {...input} />);
        await waitFor(() => expect(submit()).toBeEnabled());
        view.rerender(<RunPermissions {...input} snapshot={{ ...snapshot, seq: 11 }} />);
        expect(submit()).toBeDisabled();
        await act(async () => resolve({ result: 'task', task }));
        await waitFor(() => expect(submit()).toBeEnabled());
    });
    it('ignores metadata returned for a previously selected task', async () => {
        let resolve!: (value: ScheduleManagementResponse) => void;
        const input = props(); input.client.request = vi.fn(() => new Promise<ScheduleManagementResponse>(done => { resolve = done; }));
        const view = render(<RunPermissions {...input} />);
        const oldResolve = resolve;
        view.rerender(<RunPermissions {...input} scheduleId="task-2" />);
        await act(async () => oldResolve({ result: 'task', task }));
        expect(screen.queryByRole('button', { name: 'Submit selected permissions' })).not.toBeInTheDocument();
    });
});
