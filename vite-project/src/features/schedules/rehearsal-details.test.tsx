import { act, render, screen, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { RehearsalDetails } from './rehearsal-details';
import type { ScheduleClient } from './client';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
const details = { result: 'task_rehearsal', task: { schedule_id: 'task-1' }, rehearsal: { schedule_id: 'task-1', rehearsal_id: 'run-1', status: 'completed', prompt: 'Original requirement', started_at: null, finished_at: '2026-09-06T12:00:00Z' } };
const report = { result: 'rehearsal_permissions', rehearsal_id: 'run-1', observations: [{ tool_call_id: 'call-1', tool_name: 'read_system_info', approval_source: 'policy_auto', resources: ['selected-device'], operations: ['read'], export_destinations: [], completed_at: '2026-09-06T12:00:00Z' }], unconfirmed_tool_call_ids: ['failed'], unclassified_tool_call_ids: ['other'] };

describe('guided run details', () => {
    it('finds the saved run by task, then shows verified scope and outstanding evidence separately', async () => {
        const request = vi.fn().mockResolvedValueOnce(details).mockResolvedValueOnce(report);
        const client = { request } as unknown as ScheduleClient;
        const view = render(<RehearsalDetails client={client} scheduleId="task-1" connected zone="UTC" />);
        expect(await screen.findByText('read_system_info')).toBeInTheDocument();
        expect(screen.getByText('Resources: selected-device')).toBeInTheDocument();
        expect(screen.getByText('1 call(s) have no verified successful result.')).toBeInTheDocument();
        expect(screen.getByText('1 call(s) still need evidence classification.')).toBeInTheDocument();
        expect(request.mock.calls.map(([input]) => input)).toEqual([
            { operation: 'get_task_rehearsal', schedule_id: 'task-1' },
            { operation: 'get_rehearsal_permissions', rehearsal_id: 'run-1' },
        ]);
        view.rerender(<RehearsalDetails client={client} scheduleId="task-1" connected={false} zone="UTC" />);
        await waitFor(() => expect(screen.queryByText('read_system_info')).not.toBeInTheDocument());
    });

    it('does not fetch permissions after the dialog closes with a lookup pending', async () => {
        let resolve!: (value: unknown) => void;
        const request = vi.fn(() => new Promise(value => { resolve = value; }));
        const view = render(<RehearsalDetails client={{ request } as unknown as ScheduleClient} scheduleId="task-1" connected zone="UTC" />);
        view.unmount();
        await act(async () => resolve(details));
        expect(request).toHaveBeenCalledTimes(1);
    });

    it('shows an empty history without starting a run or requesting permissions', async () => {
        const request = vi.fn().mockResolvedValue({ ...details, rehearsal: null });
        render(<RehearsalDetails client={{ request } as unknown as ScheduleClient} scheduleId="task-1" connected zone="UTC" />);
        expect(await screen.findByText('No guided run has been recorded for this task.')).toBeInTheDocument();
        expect(request).toHaveBeenCalledTimes(1);
    });

    it('rejects a task mismatch before showing data or requesting permissions', async () => {
        const request = vi.fn().mockResolvedValue({ ...details, task: { schedule_id: 'another-task' } });
        render(<RehearsalDetails client={{ request } as unknown as ScheduleClient} scheduleId="task-1" connected zone="UTC" />);
        expect(await screen.findByRole('alert')).toBeInTheDocument();
        expect(screen.queryByText('Original requirement')).not.toBeInTheDocument();
        expect(request).toHaveBeenCalledTimes(1);
    });
});
