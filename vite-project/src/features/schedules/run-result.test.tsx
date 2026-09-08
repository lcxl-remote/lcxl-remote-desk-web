import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { RunResult } from './run-result';
import { deskErrorCodeEnum, type ScheduleView } from '@/services/types';
vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
const message = (id: string) => ({ id, text: id, role: 'assistant', turnId: 'run-turn' });
const response = (id: string, more = false, seq = 1) => ({ ok: true, json: async () => ({ code: deskErrorCodeEnum.SUCCESS, data: { sessionId: 'private-session', seq, messages: [message(id)], messagePage: { hasMore: more, nextBeforeMessageId: more ? id : null } } }) });
afterEach(() => vi.unstubAllGlobals());
describe('scheduled run conversation', () => {
    it('reads by public occurrence ids and pages the same snapshot', async () => {
        const fetch = vi.fn().mockResolvedValueOnce(response('new', true)).mockResolvedValueOnce(response('old'));
        vi.stubGlobal('fetch', fetch);
        render(<RunResult scheduleId="task" runId="run" connected onBack={() => {}} />);
        expect(await screen.findByText('new')).toBeInTheDocument();
        expect(screen.getByText('Initial turn of this run')).toBeInTheDocument();
        fireEvent.click(screen.getByText('Earlier messages'));
        expect(await screen.findByText('old')).toBeInTheDocument();
        expect(screen.getByText('new')).toBeInTheDocument();
        const url = new URL(fetch.mock.calls[1][0], 'https://example.invalid');
        expect(url.searchParams.get('scheduled_task')).toBe('task');
        expect(url.searchParams.get('scheduled_run')).toBe('run');
        expect(url.searchParams.get('message_before')).toBe('new');
        expect(url.searchParams.has('session')).toBe(false);
        expect(fetch.mock.calls[1][1].credentials).toBe('include');
    });
    it('opens the current run approval and reloads the durable decision without a new input', async () => {
        const pending = { schemaVersion: 1, requestId: 'permission', inputRevision: 1, state: 'pending', createdAt: '2026-09-06T00:00:00Z',
            items: [{ itemId: 'read', providerId: 'desktop.session', toolName: 'inspect_desktop_session', reason: 'Read this device', expectedEffect: 'read_device',
                resourceScope: ['target:device'], operationScope: ['observe'], exportDestinations: [], suggestedTtlSeconds: 60, suggestedMaxUses: 1 }] };
        const saved = (state: string, seq: number) => ({ ok: true, json: async () => ({ code: deskErrorCodeEnum.SUCCESS,
            data: { sessionId: 'original-session', requestId: 'run', seq, inputRevision: 1, messages: [message('Saved original conversation')],
                messagePage: { hasMore: false }, permissionRequests: [{ ...pending, state }] } }) });
        const fetch = vi.fn().mockResolvedValueOnce(saved('pending', 1))
            .mockResolvedValueOnce({ ok: true, json: async () => ({ code: deskErrorCodeEnum.SUCCESS, data: { state: 'approved' } }) })
            .mockResolvedValueOnce(saved('approved', 2));
        vi.stubGlobal('fetch', fetch);
        const client = { request: vi.fn().mockResolvedValue({ result: 'task', task: { schedule_id: 'task', active_run_id: 'run', target_device_id: 'device', status: 'triggered' } as ScheduleView }) };
        render(<RunResult scheduleId="task" runId="run" client={client} connectionIds={{ device: 'online-host' }} connected onBack={() => {}} />);
        await screen.findByText('Saved original conversation');
        await waitFor(() => expect(screen.getByRole('button', { name: 'Submit selected permissions' })).toBeEnabled());
        fireEvent.click(screen.getByRole('button', { name: 'Submit selected permissions' }));
        await waitFor(() => expect(fetch).toHaveBeenCalledTimes(3));
        await waitFor(() => expect(screen.queryByRole('button', { name: 'Submit selected permissions' })).not.toBeInTheDocument());
        expect(JSON.parse(fetch.mock.calls[1][1].body)).toMatchObject({ session: 'original-session', expectedRunRequestId: 'run', requestId: 'permission', connection: 'online-host' });
        expect(fetch.mock.calls[2][0]).toContain('scheduled_task=task&scheduled_run=run');
        expect(screen.getByText('Saved original conversation')).toBeInTheDocument();
        expect(await screen.findByText('Decision recorded. The conversation has been refreshed to check execution progress.')).toBeInTheDocument();
    });
    it('clears sensitive content when refresh loses access', async () => {
        const fetch = vi.fn().mockResolvedValueOnce(response('saved answer')).mockResolvedValueOnce({ ok: true, json: async () => ({ code: deskErrorCodeEnum.PERMISSION_ERROR }) });
        vi.stubGlobal('fetch', fetch);
        render(<RunResult scheduleId="task" runId="run" connected onBack={() => {}} />);
        expect(await screen.findByText('saved answer')).toBeInTheDocument();
        fireEvent.click(screen.getByText('Refresh'));
        expect(await screen.findByRole('alert')).toBeInTheDocument();
        expect(screen.queryByText('saved answer')).not.toBeInTheDocument();
    });
    it('shows approval continuation messages without mislabeling them as the initial turn', async () => {
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({
            code: deskErrorCodeEnum.SUCCESS,
            data: { sessionId: 'private-session', seq: 2, messages: [
                message('initial answer'),
                { ...message('answer after approval'), turnId: 'permission-resume-decision' },
            ], messagePage: { hasMore: false } },
        }) }));
        render(<RunResult scheduleId="task" runId="run" connected onBack={() => {}} />);
        expect(await screen.findByText('answer after approval')).toBeInTheDocument();
        expect(screen.getAllByText('Initial turn of this run')).toHaveLength(1);
        expect(screen.getByText('answer after approval').closest('article')).not.toHaveTextContent('Initial turn of this run');
    });
    it('does not merge an older page from a newer session revision', async () => {
        vi.stubGlobal('fetch', vi.fn().mockResolvedValueOnce(response('new', true)).mockResolvedValueOnce(response('old', false, 2)));
        render(<RunResult scheduleId="task" runId="run" connected onBack={() => {}} />);
        await screen.findByText('new'); fireEvent.click(screen.getByText('Earlier messages'));
        expect(await screen.findByRole('alert')).toBeInTheDocument();
        expect(screen.queryByText('old')).not.toBeInTheDocument();
    });
    it('ignores a late response after disconnect', async () => {
        let resolve!: (value: unknown) => void;
        vi.stubGlobal('fetch', vi.fn(() => new Promise(value => { resolve = value; })));
        const view = render(<RunResult scheduleId="task" runId="run" connected onBack={() => {}} />);
        view.rerender(<RunResult scheduleId="task" runId="run" connected={false} onBack={() => {}} />);
        await act(async () => resolve(response('late answer')));
        expect(screen.queryByText('late answer')).not.toBeInTheDocument();
    });
});
