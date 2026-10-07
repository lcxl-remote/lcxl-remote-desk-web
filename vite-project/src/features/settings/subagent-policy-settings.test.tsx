import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import SubAgentPolicySettings from './subagent-policy-settings';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const policy = (revision: number, unfinished = 2) => ({
    schemaVersion: 1, revision, limits: { maxUnfinishedPerRoot: unfinished },
});
const response = (data: unknown) => ({ ok: true, json: async () => ({ success: true, data }) });
afterEach(() => vi.unstubAllGlobals());

describe('subagent policy settings', () => {
    it('renders and saves only the unfinished limit with the loaded revision', async () => {
        const fetchMock = vi.fn().mockResolvedValueOnce(response(policy(4)))
            .mockResolvedValueOnce(response(policy(5, 8)));
        vi.stubGlobal('fetch', fetchMock);
        render(<SubAgentPolicySettings />);
        const unfinished = await screen.findByLabelText('pages.aiAssistant.subagentPolicyUnfinished');
        await waitFor(() => expect(unfinished).toHaveValue(2));
        expect(screen.getAllByRole('spinbutton')).toHaveLength(1);
        fireEvent.change(unfinished, { target: { value: '8' } });
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAssistant.goalBudgetSave' }));
        await screen.findByText('pages.aiAssistant.subagentPolicySaved');
        expect(fetchMock.mock.calls[1][0]).toBe('/api/admin/system/subagent-policy');
        expect(JSON.parse(fetchMock.mock.calls[1][1].body)).toEqual({ expectedRevision: 4,
            limits: { maxUnfinishedPerRoot: 8 } });
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
