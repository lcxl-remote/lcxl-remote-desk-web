import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { RunHistory } from './run-history';
import { deskErrorCodeEnum } from '@/services/types';
import type { ScheduleClient } from './client';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
const run = (run_id: string, status = 'succeeded') => ({ run_id, status, source: 'calendar', requested_at: '2026-10-01T12:00:00.000Z', scheduled_at: null, started_at: null, finished_at: null, cancel_requested_at: null, missed_count: 0, issue: null });
const page = (runs: ReturnType<typeof run>[], next_cursor: string | null = null, schedule_id = 'task-1') => ({ result: 'runs', schedule_id, runs, next_cursor });

afterEach(() => vi.unstubAllGlobals());

describe('run history', () => {
    it('loads older occurrences with the returned cursor and refreshes from the newest', async () => {
        const request = vi.fn().mockResolvedValueOnce(page([run('new')], 'new')).mockResolvedValueOnce(page([run('old', 'failed')])).mockResolvedValueOnce(page([]));
        render(<RunHistory client={{ request } as unknown as ScheduleClient} scheduleId="task-1" connected zone="UTC" />);
        expect(await screen.findByText('Succeeded')).toBeInTheDocument();
        fireEvent.click(screen.getByText('Older runs'));
        expect(await screen.findByText('Failed')).toBeInTheDocument();
        expect(screen.getByText('Succeeded')).toBeInTheDocument();
        expect(request.mock.calls[1][0]).toEqual({ operation: 'list_runs', schedule_id: 'task-1', before: 'new', limit: 25 });
        fireEvent.click(screen.getByText('Refresh'));
        expect(await screen.findByText('No runs yet.')).toBeInTheDocument();
        expect(screen.queryByText('Failed')).not.toBeInTheDocument();
        expect(request.mock.calls[2][0].before).toBeNull();
    });
    it('clears history on disconnect and ignores the pending response', async () => {
        let resolve!: (value: unknown) => void;
        const request = vi.fn(() => new Promise(value => { resolve = value; }));
        const client = { request } as unknown as ScheduleClient;
        const view = render(<RunHistory client={client} scheduleId="task-1" connected zone="UTC" />);
        view.rerender(<RunHistory client={client} scheduleId="task-1" connected={false} zone="UTC" />);
        await act(async () => resolve(page([run('late')])));
        expect(screen.queryByText('Succeeded')).not.toBeInTheDocument();
        expect(request).toHaveBeenCalledTimes(1);
    });
    it('ignores the old task response after switching tasks', async () => {
        let resolve!: (value: unknown) => void;
        const request = vi.fn().mockImplementationOnce(() => new Promise(value => { resolve = value; })).mockResolvedValueOnce(page([run('current', 'running')], null, 'task-2'));
        const client = { request } as unknown as ScheduleClient;
        const view = render(<RunHistory client={client} scheduleId="task-1" connected zone="UTC" />);
        view.rerender(<RunHistory client={client} scheduleId="task-2" connected zone="UTC" />);
        expect(await screen.findByText('Running')).toBeInTheDocument();
        await act(async () => resolve(page([run('old')])));
        expect(screen.queryByText('Succeeded')).not.toBeInTheDocument();
        expect(screen.getByText('Running')).toBeInTheDocument();
    });
    it('renders the closed failure reason independently from the terminal status', async () => {
        const failed = { ...run('failed', 'failed'), issue: { kind: 'agent', error: 'permission_denied' } };
        const request = vi.fn().mockResolvedValue({ result: 'runs', schedule_id: 'task-1', runs: [failed], next_cursor: null });
        render(<RunHistory client={{ request } as unknown as ScheduleClient} scheduleId="task-1" connected zone="UTC" />);
        expect(await screen.findByText('Reason: Permission denied.')).toBeInTheDocument();
        expect(screen.getByText('Failed')).toBeInTheDocument();
    });
    it('opens the selected occurrence conversation without executing the task', async () => {
        const started = { ...run('run'), started_at: '2026-10-01T12:00:00.000Z' };
        const request = vi.fn().mockResolvedValue({ result: 'runs', schedule_id: 'task-1', runs: [started], next_cursor: null });
        const fetch = vi.fn().mockResolvedValue({ ok: true, json: async () => ({ code: deskErrorCodeEnum.SUCCESS, data: { sessionId: 'original-session', seq: 1, messages: [{ id: 'answer', role: 'assistant', text: 'Original answer', turnId: 'run-turn' }], messagePage: { hasMore: false } } }) });
        vi.stubGlobal('fetch', fetch);
        render(<RunHistory client={{ request } as unknown as ScheduleClient} scheduleId="task-1" connected zone="UTC" />);
        fireEvent.click(await screen.findByText('View conversation'));
        expect(await screen.findByText('Original answer')).toBeInTheDocument();
        expect(request).toHaveBeenCalledTimes(1);
        expect(fetch.mock.calls[0][0]).toContain('scheduled_run=run');
        fireEvent.click(screen.getByText('Back to run history'));
        expect(screen.getByText('Succeeded')).toBeInTheDocument();
    });
    it('rejects a response for a different task', async () => {
        const request = vi.fn().mockResolvedValue(page([run('foreign')], null, 'task-2'));
        render(<RunHistory client={{ request } as unknown as ScheduleClient} scheduleId="task-1" connected zone="UTC" />);
        await waitFor(() => expect(screen.getByRole('alert')).toBeInTheDocument());
        expect(screen.queryByText('Succeeded')).not.toBeInTheDocument();
    });
});
