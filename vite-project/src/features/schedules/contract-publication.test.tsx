import { StrictMode } from 'react';
import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import type { ScheduleManagementResponse } from '@/services/types';
import { ContractPublication } from './contract-publication';
import { ScheduleClient, ScheduleRequestError } from './client';
vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
type Review = Extract<ScheduleManagementResponse, { result: 'task_contract' }>;
const review = { result: 'task_contract', task_revision: 1, prompt_sha256: 'a'.repeat(64), contract_sha256: 'b'.repeat(64),
    task: { schedule_id: 'task', revision: 3, kind: 'fresh_task', status: 'draft', target_device_id: 'device', active_run_id: null },
    contract: { contract_revision: 2, task_revision: 1, target_device_id: 'device', prompt_sha256: 'a'.repeat(64) },
} as Review;
const rehearsal = { result: 'task_rehearsal', task: review.task, rehearsal: { schedule_id: 'task', task_revision: 1, status: 'completed', rehearsal_id: 'guided-run' } };
function setup(request: ReturnType<typeof vi.fn>, value = review) {
    const onPublished = vi.fn();
    const view = render(<StrictMode><ContractPublication client={{ request } as unknown as ScheduleClient} review={value} connected onPublished={onPublished} /></StrictMode>);
    return { ...view, onPublished };
}
describe('explicit task publication', () => {
    it('requires guided-run verification and explicit consent, and freezes retries after uncertainty', async () => {
        const request = vi.fn().mockResolvedValueOnce(rehearsal).mockRejectedValueOnce(new ScheduleRequestError('timeout'))
            .mockResolvedValueOnce({ result: 'task', task: { ...review.task, status: 'active', revision: 4 } });
        const { onPublished } = setup(request);
        expect(request).not.toHaveBeenCalled();
        fireEvent.click(screen.getByText('Check guided run for publication'));
        const publish = await screen.findByText('Approve and publish');
        expect(publish).toBeDisabled();
        expect(request).toHaveBeenCalledTimes(1);
        fireEvent.click(screen.getByRole('checkbox'));
        fireEvent.click(publish);
        await screen.findByRole('alert');
        expect(onPublished).not.toHaveBeenCalled();
        await waitFor(() => expect(publish).not.toBeDisabled());
        fireEvent.click(publish);
        await waitFor(() => expect(onPublished).toHaveBeenCalledTimes(1));
        expect(request.mock.calls[1][0]).toEqual(request.mock.calls[2][0]);
        expect(request.mock.calls[1][0]).toMatchObject({ operation: 'publish_task', expected_revision: 3,
            contract_revision: 2, contract_sha256: review.contract_sha256, rehearsal_run_id: 'guided-run', expires_at: null });
    });
    it('rejects a stale guided-run lookup before exposing confirmation', async () => {
        const request = vi.fn().mockResolvedValue({ ...rehearsal, task: { ...review.task, revision: 4 } });
        setup(request);
        fireEvent.click(screen.getByText('Check guided run for publication'));
        await screen.findByRole('alert');
        expect(screen.queryByRole('checkbox')).not.toBeInTheDocument();
        expect(request).toHaveBeenCalledTimes(1);
    });
    it('does not offer publication for a contract from an older task revision', () => {
        const request = vi.fn();
        setup(request, { ...review, task_revision: 2 });
        expect(screen.queryByRole('button')).not.toBeInTheDocument();
        expect(request).not.toHaveBeenCalled();
    });
    it('does not apply a result after the review is closed', async () => {
        let resolve!: (value: unknown) => void;
        const request = vi.fn().mockResolvedValueOnce(rehearsal).mockImplementationOnce(() => new Promise(value => { resolve = value; }));
        const { unmount, onPublished } = setup(request);
        fireEvent.click(screen.getByText('Check guided run for publication'));
        await screen.findByText('Approve and publish');
        fireEvent.click(screen.getByRole('checkbox'));
        fireEvent.click(screen.getByText('Approve and publish'));
        unmount();
        await act(async () => resolve({ result: 'task', task: { ...review.task, status: 'active' } }));
        expect(onPublished).not.toHaveBeenCalled();
    });
});
