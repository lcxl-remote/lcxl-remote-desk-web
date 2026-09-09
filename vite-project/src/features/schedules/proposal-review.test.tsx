import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';
import { ProposalReview } from './proposal-review';
import type { ScheduleClient } from './client';
const translate = vi.hoisted(() => (key: string, args?: { seconds: number }) => args ? `${key}:${args.seconds}` : key);
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: translate, i18n: { language: 'en' } }) }));
vi.mock('./contract-review', () => ({ ContractReview: () => null }));
vi.mock('./rehearsal-details', () => ({ RehearsalDetails: () => null }));
afterEach(cleanup);
it('reviews server data and activates only the reviewed task revision after a click', async () => {
    const task = { schedule_id: 'task', revision: 7, kind: 'conversation_resume', status: 'pending_review', title: 'Hello later', prompt: 'hello', spec: { schema_version: 1, rule: { kind: 'after_confirmation', delay_seconds: 300 } } };
    const request = vi.fn(async (input: { operation: string }) => ({ result: 'task', task: input.operation === 'get' ? task : { ...task, revision: 8, status: 'active', spec: { schema_version: 1, rule: { kind: 'once', at: '2026-09-10T12:00:00Z' } } } }));
    const changed = vi.fn();
    render(<ProposalReview client={{ request } as unknown as ScheduleClient} scheduleId="task" connected zone="UTC" assistantPaths={{}} onChanged={changed} />);
    await screen.findByText('schedules.proposal.afterConfirmation:300');
    expect(request.mock.calls).toEqual([[{ operation: 'get', schedule_id: 'task' }]]);
    fireEvent.click(screen.getByRole('button', { name: 'schedules.activateResume' }));
    await waitFor(() => expect(changed).toHaveBeenCalledTimes(1));
    expect(request.mock.calls[1]).toEqual([{ operation: 'activate_conversation_resume', schedule_id: 'task', expected_revision: 7 }]);
    expect(screen.getByText('schedules.status.active')).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'schedules.activateResume' })).toBeNull();
});

it('rejects the exact reviewed draft and exposes explicit approval choices', async () => {
    const task = { schedule_id: 'task', revision: 7, kind: 'conversation_resume', status: 'pending_review', title: 'Hello later', prompt: 'hello', spec: { schema_version: 1, rule: { kind: 'after_confirmation', delay_seconds: 60 } } };
    const request = vi.fn(async (input: { operation: string }) => ({ result: 'task', task: input.operation === 'get' ? task : { ...task, revision: 8, status: 'deleted' } }));
    const changed = vi.fn();
    render(<ProposalReview client={{ request } as unknown as ScheduleClient} scheduleId="task" connected zone="UTC" assistantPaths={{}} onChanged={changed} approvalCard />);
    await screen.findByRole('button', { name: 'schedules.proposal.approve' });
    fireEvent.click(screen.getByRole('button', { name: 'schedules.proposal.reject' }));
    await waitFor(() => expect(changed).toHaveBeenCalledTimes(1));
    expect(request.mock.calls[1]).toEqual([{ operation: 'delete', schedule_id: 'task', expected_revision: 7 }]);
    expect(screen.queryByRole('button', { name: 'schedules.proposal.approve' })).toBeNull();
});

it('waits for explicit rejection and server acknowledgement in the approval card', async () => {
    const task = { schedule_id: 'task', revision: 7, kind: 'conversation_resume', status: 'pending_review', title: 'Later', prompt: 'hello', spec: { schema_version: 1, rule: { kind: 'after_confirmation', delay_seconds: 60 } } };
    let finish!: (value: unknown) => void;
    const request = vi.fn((input: { operation: string }) => input.operation === 'get' ? Promise.resolve({ result: 'task', task }) : new Promise(resolve => { finish = resolve; }));
    const changed = vi.fn();
    const props = { client: { request } as unknown as ScheduleClient, scheduleId: 'task', connected: true, zone: 'UTC', assistantPaths: {}, onChanged: changed, approvalCard: true };
    render(<ProposalReview {...props} />);
    await screen.findByRole('button', { name: 'schedules.proposal.approve' });
    expect(screen.queryByRole('dialog')).toBeNull();
    fireEvent.keyDown(document.body, { key: 'Escape' });
    expect(request).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole('button', { name: 'schedules.proposal.reject' }));
    await waitFor(() => expect(request).toHaveBeenCalledWith({ operation: 'delete', schedule_id: 'task', expected_revision: 7 }));
    expect(changed).not.toHaveBeenCalled();
    finish({ result: 'task', task: { ...task, status: 'deleted', revision: 8 } });
    await waitFor(() => expect(changed).toHaveBeenCalledTimes(1));
});
