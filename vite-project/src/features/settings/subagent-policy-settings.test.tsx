import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import SubAgentPolicySettings from './subagent-policy-settings';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const policy = (revision: number, unfinished = 2, duration = 120) => ({
    schemaVersion: 2, revision, limits: { maxUnfinishedPerRoot: unfinished, maxTaskDurationMinutes: duration },
});
const response = (data: unknown) => ({ ok: true, json: async () => ({ success: true, data }) });
afterEach(() => vi.unstubAllGlobals());

describe('subagent policy settings', () => {
    it('saves the unfinished limit and task duration with the loaded revision', async () => {
        const fetchMock = vi.fn().mockResolvedValueOnce(response(policy(4)))
            .mockResolvedValueOnce(response(policy(5, 8, 45)));
        vi.stubGlobal('fetch', fetchMock);
        render(<SubAgentPolicySettings />);
        const unfinished = await screen.findByLabelText('pages.aiAssistant.subagentPolicyUnfinished');
        await waitFor(() => expect(unfinished).toHaveValue(2));
        const duration = screen.getByLabelText('pages.aiAssistant.subagentPolicyTaskDuration');
        expect(duration).toHaveValue(120);
        fireEvent.change(unfinished, { target: { value: '8' } });
        fireEvent.change(duration, { target: { value: '45' } });
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAssistant.goalBudgetSave' }));
        await screen.findByText('pages.aiAssistant.subagentPolicySaved');
        expect(fetchMock.mock.calls[1][0]).toBe('/api/admin/system/subagent-policy');
        expect(JSON.parse(fetchMock.mock.calls[1][1].body)).toEqual({ expectedRevision: 4,
            limits: { maxUnfinishedPerRoot: 8, maxTaskDurationMinutes: 45 } });
    });

    it('rejects zero, fractional and out-of-range values without submitting', async () => {
        const fetchMock = vi.fn().mockResolvedValue(response(policy(0)));
        vi.stubGlobal('fetch', fetchMock);
        render(<SubAgentPolicySettings />);
        const unfinished = await screen.findByLabelText('pages.aiAssistant.subagentPolicyUnfinished');
        await waitFor(() => expect(unfinished).toHaveValue(2));
        const save = screen.getByRole('button', { name: 'pages.aiAssistant.goalBudgetSave' });
        for (const value of ['0', '1.5', '33', '']) {
            fireEvent.change(unfinished, { target: { value } });
            expect(save).toBeDisabled();
        }
        fireEvent.change(unfinished, { target: { value: '32' } });
        const duration = screen.getByLabelText('pages.aiAssistant.subagentPolicyTaskDuration');
        for (const value of ['0', '1.5', '4294967296', '']) {
            fireEvent.change(duration, { target: { value } });
            expect(save).toBeDisabled();
        }
        fireEvent.change(duration, { target: { value: '1' } });
        expect(save).toBeEnabled();
        expect(fetchMock).toHaveBeenCalledTimes(1);
    });

    it('requires reload after a revision conflict and retains the entered values', async () => {
        const fetchMock = vi.fn().mockResolvedValueOnce(response(policy(0)))
            .mockResolvedValueOnce({ ok: true, json: async () => ({ success: false, message: 'reload before retrying' }) })
            .mockResolvedValueOnce(response(policy(1, 6)));
        vi.stubGlobal('fetch', fetchMock);
        render(<SubAgentPolicySettings />);
        const unfinished = await screen.findByLabelText('pages.aiAssistant.subagentPolicyUnfinished');
        await waitFor(() => expect(unfinished).toHaveValue(2));
        fireEvent.change(unfinished, { target: { value: '8' } });
        const save = screen.getByRole('button', { name: 'pages.aiAssistant.goalBudgetSave' });
        fireEvent.click(save);
        expect(await screen.findByRole('alert')).toHaveTextContent('reload before retrying');
        expect(save).toBeDisabled();
        expect(unfinished).toHaveValue(8);
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAssistant.goalBudgetReload' }));
        await waitFor(() => expect(unfinished).toHaveValue(6));
    });
});
