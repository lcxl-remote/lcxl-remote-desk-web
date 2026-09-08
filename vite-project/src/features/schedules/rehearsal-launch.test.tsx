import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import type { ScheduleManagementResponse } from '@/services/types';
import { ScheduleClient, ScheduleRequestError } from './client';
import { RehearsalLaunch } from './rehearsal-launch';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
type Details = Extract<ScheduleManagementResponse, { result: 'task_rehearsal' }>;
const details = { result: 'task_rehearsal', task: { schedule_id: 'task', revision: 2, kind: 'fresh_task', status: 'draft', active_run_id: null, pause_reasons: [] }, rehearsal: null } as Details;
const reserved = { result: 'rehearsal', task: { ...details.task, revision: 3, status: 'rehearsing' }, rehearsal: { schedule_id: 'task', rehearsal_id: 'run / 1', status: 'pending' } };

describe('guided run preparation', () => {
    it('reserves only on a user click and reuses the request after an uncertain result', async () => {
        const request = vi.fn().mockRejectedValueOnce(new ScheduleRequestError('timeout')).mockResolvedValueOnce(reserved);
        const onReserved = vi.fn();
        const client = { request } as unknown as ScheduleClient;
        const view = render(<RehearsalLaunch details={details} client={client} connected path="/desk/live/assistant" onReserved={onReserved} />);
        expect(request).not.toHaveBeenCalled();
        fireEvent.click(screen.getByRole('button', { name: 'Prepare guided run' }));
        expect(await screen.findByRole('alert')).toBeInTheDocument();
        fireEvent.click(screen.getByRole('button', { name: 'Prepare guided run' }));
        await waitFor(() => expect(onReserved).toHaveBeenCalledTimes(1));
        expect(request.mock.calls[0][0]).toEqual(request.mock.calls[1][0]);
        expect(request.mock.calls[0][0]).toMatchObject({ operation: 'reserve_rehearsal', schedule_id: 'task', expected_revision: 2 });
        view.rerender(<RehearsalLaunch key="reserved" details={onReserved.mock.calls[0][0]} client={client} connected path="/desk/live/assistant" onReserved={onReserved} />);
        expect(screen.getByRole('link', { name: 'Open guided run' })).toHaveAttribute('href', '/desk/live/assistant?rehearsal=run%20%2F%201');
        expect(request).toHaveBeenCalledTimes(2);
    });
    it('ignores a reservation response after the dialog closes', async () => {
        let resolve!: (value: unknown) => void;
        const request = vi.fn(() => new Promise(value => { resolve = value; }));
        const onReserved = vi.fn();
        const view = render(<RehearsalLaunch details={details} client={{ request } as unknown as ScheduleClient} connected path="/desk/live/assistant" onReserved={onReserved} />);
        fireEvent.click(screen.getByRole('button', { name: 'Prepare guided run' }));
        view.unmount();
        await act(async () => resolve(reserved));
        expect(onReserved).not.toHaveBeenCalled();
    });
    it('cancels an unstarted reservation even when the device is offline', async () => {
        const pendingDetails = { ...details, task: reserved.task, rehearsal: reserved.rehearsal } as Details;
        const cancelled = { ...reserved, task: { ...details.task, revision: 4 }, rehearsal: { ...reserved.rehearsal, status: 'cancelled' } };
        const request = vi.fn().mockRejectedValueOnce(new ScheduleRequestError('timeout')).mockResolvedValueOnce(cancelled);
        const onReserved = vi.fn();
        render(<RehearsalLaunch details={pendingDetails} client={{ request } as unknown as ScheduleClient} connected onReserved={onReserved} />);
        fireEvent.click(screen.getByRole('button', { name: 'Cancel unstarted guided run' }));
        expect(await screen.findByRole('alert')).toBeInTheDocument();
        fireEvent.click(screen.getByRole('button', { name: 'Cancel unstarted guided run' }));
        await waitFor(() => expect(onReserved).toHaveBeenCalledTimes(1));
        expect(request.mock.calls[0][0]).toEqual({ operation: 'cancel_pending_rehearsal', rehearsal_id: 'run / 1', expected_revision: 3 });
        expect(request.mock.calls[1][0]).toEqual(request.mock.calls[0][0]);
        expect(onReserved.mock.calls[0][0].rehearsal.status).toBe('cancelled');
    });
    it('does not offer reservation cancellation for a running rehearsal', () => {
        const running = { ...details, task: reserved.task, rehearsal: { ...reserved.rehearsal, status: 'running' } } as Details;
        render(<RehearsalLaunch details={running} client={{ request: vi.fn() } as unknown as ScheduleClient} connected onReserved={vi.fn()} />);
        expect(screen.queryByRole('button', { name: 'Cancel unstarted guided run' })).not.toBeInTheDocument();
    });
    it('rejects a cancellation response for another rehearsal', async () => {
        const pendingDetails = { ...details, task: reserved.task, rehearsal: reserved.rehearsal } as Details;
        const request = vi.fn().mockResolvedValue({ ...reserved, rehearsal: { ...reserved.rehearsal, rehearsal_id: 'another', status: 'cancelled' } });
        const onReserved = vi.fn();
        render(<RehearsalLaunch details={pendingDetails} client={{ request } as unknown as ScheduleClient} connected onReserved={onReserved} />);
        fireEvent.click(screen.getByRole('button', { name: 'Cancel unstarted guided run' }));
        expect(await screen.findByRole('alert')).toBeInTheDocument();
        expect(onReserved).not.toHaveBeenCalled();
    });
    it('does not prepare a run while its device has no unambiguous online route', () => {
        const request = vi.fn();
        render(<RehearsalLaunch details={details} client={{ request } as unknown as ScheduleClient} connected onReserved={vi.fn()} />);
        expect(screen.getByRole('button', { name: 'Prepare guided run' })).toBeDisabled();
        expect(request).not.toHaveBeenCalled();
    });
});
