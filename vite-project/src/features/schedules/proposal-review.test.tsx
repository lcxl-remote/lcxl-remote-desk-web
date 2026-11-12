import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';
import { ProposalReview } from './proposal-review';
import type { ScheduleClient } from './client';
const translate = vi.hoisted(() => (key: string, args?: { count: number }) => args ? `${key}:${args.count}` : key);
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: translate, i18n: { language: 'en' } }) }));
afterEach(cleanup);

const proposal = { schedule_id: 'task', revision: 7, kind: 'fresh_task', status: 'pending_review', title: 'Morning check', prompt: 'Report status',
    spec: { schema_version: 1, rule: { kind: 'interval', every_seconds: 3600 } }, upcoming_runs: [] };

it('approves only the reviewed proposal revision after a click', async () => {
    const request = vi.fn(async (input: { operation: string }) => ({ result: 'task', task: input.operation === 'get' ? proposal : { ...proposal, revision: 8, status: 'active' } }));
    const changed = vi.fn();
    render(<ProposalReview client={{ request } as unknown as ScheduleClient} scheduleId="task" connected zone="UTC" onChanged={changed} />);
    await screen.findByText('schedules.everySeconds:3600');
    expect(request.mock.calls).toEqual([[{ operation: 'get', schedule_id: 'task' }]]);
    fireEvent.click(screen.getByRole('button', { name: 'schedules.proposal.approve' }));
    await waitFor(() => expect(changed).toHaveBeenCalledTimes(1));
    expect(request.mock.calls[1]).toEqual([{ operation: 'decide_schedule_proposal', schedule_id: 'task', expected_revision: 7, approve: true }]);
    expect(screen.getByText('schedules.status.active')).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'schedules.proposal.approve' })).toBeNull();
});

it('rejects the exact reviewed proposal and waits for the server acknowledgement', async () => {
    let finish!: (value: unknown) => void;
    const request = vi.fn((input: { operation: string }) => input.operation === 'get' ? Promise.resolve({ result: 'task', task: proposal }) : new Promise(resolve => { finish = resolve; }));
    const changed = vi.fn();
    render(<ProposalReview client={{ request } as unknown as ScheduleClient} scheduleId="task" connected zone="UTC" onChanged={changed} approvalCard />);
    fireEvent.click(await screen.findByRole('button', { name: 'schedules.proposal.reject' }));
    await waitFor(() => expect(request).toHaveBeenCalledWith({ operation: 'decide_schedule_proposal', schedule_id: 'task', expected_revision: 7, approve: false }));
    expect(changed).not.toHaveBeenCalled();
    finish({ result: 'task', task: { ...proposal, status: 'deleted', revision: 8 } });
    await waitFor(() => expect(changed).toHaveBeenCalledTimes(1));
});

it('does not render a decided proposal in the conversation', async () => {
    const request = vi.fn(async () => ({ result: 'task', task: { ...proposal, status: 'active' } }));
    const { container } = render(<ProposalReview client={{ request } as unknown as ScheduleClient} scheduleId="task" connected zone="UTC" onChanged={() => {}} approvalCard />);
    await waitFor(() => expect(request).toHaveBeenCalled());
    await waitFor(() => expect(container.textContent).toBe(''));
});

it('reports the refreshed server state without treating it as a new decision', async () => {
    const request = vi.fn(async () => ({ result: 'task', task: proposal }));
    const loaded = vi.fn();
    const changed = vi.fn();
    render(<ProposalReview client={{ request } as unknown as ScheduleClient} scheduleId="task" connected zone="UTC"
        onLoaded={loaded} onChanged={changed} approvalCard />);
    await waitFor(() => expect(loaded).toHaveBeenCalledWith(proposal));
    expect(changed).not.toHaveBeenCalled();
});
