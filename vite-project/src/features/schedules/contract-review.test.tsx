import { act, fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { ContractReview } from './contract-review';
import { ScheduleClient } from './client';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
const review = { result: 'task_contract', task: { schedule_id: 'task-1' }, task_revision: 1, prompt_sha256: 'a'.repeat(64), contract_sha256: 'b'.repeat(64), contract: {
    schedule_id: 'task-1', contract_revision: 2, budget: { max_runs_per_utc_day: 4, max_calls_per_run: 10, max_model_tokens_per_run: 1000, max_runtime_seconds: 60 },
    steps: [{ step_id: 'send', binding: { kind: 'send_message', destination: { account_id: 'owner-account', profile_id: 'browser-profile', channel: 'email', scope: { kind: 'web_origin', origin: { kind: 'https', host_ascii: 'mail.google.com', port: 443 } }, recipients: [{ role: 'to', stable_id: 'recipient-1', canonical_address: 'test@example.invalid' }] } } }],
    permissions: [{ rule_id: 'send', tool_name: 'send_reviewed_message', automatic: { resources: ['mailbox:owner'], operations: ['send'] } }],
} };
function client(request: ReturnType<typeof vi.fn>) { return { request } as unknown as ScheduleClient; }
describe('task contract review', () => {
    it('generates an unapproved contract only after an explicit click', async () => {
        const task = { schedule_id: 'task-1', kind: 'fresh_task', status: 'awaiting_authorization', revision: 4 };
        const request = vi.fn().mockResolvedValueOnce({ ...review, task, contract: null, contract_sha256: null }).mockResolvedValueOnce({ ...review, task: { ...task, revision: 5 } });
        const updated = vi.fn();
        render(<ContractReview client={client(request)} scheduleId="task-1" connected onPublished={updated} />);
        const button = await screen.findByText('Generate review contract from guided run');
        expect(request).toHaveBeenCalledTimes(1);
        fireEvent.click(button);
        expect(await screen.findByText(/owner-account/)).toBeInTheDocument();
        expect(request.mock.calls[1][0]).toEqual({ operation: 'generate_task_contract', schedule_id: 'task-1', expected_revision: 4 });
        expect(request).toHaveBeenCalledTimes(2);
        expect(updated).toHaveBeenCalledWith({ ...task, revision: 5 });
    });
    it('shows destination and budgets while issuing only a read request', async () => {
        const request = vi.fn().mockResolvedValue(review);
        render(<ContractReview client={client(request)} scheduleId="task-1" connected />);
        expect(await screen.findByText(/owner-account/)).toBeInTheDocument();
        expect(screen.getByText(/test@example.invalid/)).toBeInTheDocument();
        expect(screen.getByText(/https mail.google.com:443/)).toBeInTheDocument();
        expect(screen.getByText(/browser-profile/)).toBeInTheDocument();
        expect(screen.getByText('Up to 4 runs per UTC day')).toBeInTheDocument();
        expect(screen.getByText('Review task permissions, fixed steps and budgets. Contract changes require a new publication confirmation.')).toBeInTheDocument();
        expect(request.mock.calls).toEqual([[{ operation: 'get_task_contract', schedule_id: 'task-1' }]]);
        expect(screen.getAllByRole('button')).toHaveLength(1);
    });
    it('shows missing contracts without creating one or granting authority', async () => {
        const request = vi.fn().mockResolvedValue({ ...review, contract: null, contract_sha256: null });
        render(<ContractReview client={client(request)} scheduleId="task-1" connected />);
        expect(await screen.findByText('No task contract has been saved.')).toBeInTheDocument();
        expect(request).toHaveBeenCalledTimes(1);
    });
    it('rejects another task and clears stale content after disconnect', async () => {
        const request = vi.fn().mockResolvedValue({ ...review, task: { schedule_id: 'other' } });
        const transport = client(request);
        const { rerender } = render(<ContractReview client={transport} scheduleId="task-1" connected />);
        expect(await screen.findByRole('alert')).toBeInTheDocument();
        expect(screen.queryByText(/owner-account/)).not.toBeInTheDocument();
        request.mockResolvedValue(review);
        await act(async () => rerender(<ContractReview client={transport} scheduleId="task-2" connected={false} />));
        expect(screen.queryByRole('alert')).not.toBeInTheDocument();
        expect(request).toHaveBeenCalledTimes(1);
    });
});
