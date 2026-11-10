import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import AutomationPolicySettings from './automation-policy-settings';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const policy = (revision: number, maxAutoTurnsPerChain = 20) => ({ schemaVersion: 1, revision, maxAutoTurnsPerChain });
const response = (data: unknown) => ({ ok: true, json: async () => ({ success: true, data }) });
afterEach(() => vi.unstubAllGlobals());

describe('automation policy settings', () => {
    it('saves the per-request limit with the loaded revision', async () => {
        const fetchMock = vi.fn().mockResolvedValueOnce(response(policy(3)))
            .mockResolvedValueOnce(response(policy(4, 8)));
        vi.stubGlobal('fetch', fetchMock);
        render(<AutomationPolicySettings />);
        const limit = await screen.findByLabelText('pages.aiAssistant.automationPolicyMaxTurns');
        await waitFor(() => expect(limit).toHaveValue(20));
        fireEvent.change(limit, { target: { value: '8' } });
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAssistant.goalBudgetSave' }));
        await screen.findByText('pages.aiAssistant.automationPolicySaved');
        expect(fetchMock.mock.calls[1][0]).toBe('/api/admin/system/automation-policy');
        expect(JSON.parse(fetchMock.mock.calls[1][1].body)).toEqual({ expectedRevision: 3, maxAutoTurnsPerChain: 8 });
        expect(limit).toHaveValue(8);
    });

    it('rejects values outside one to fifty without submitting', async () => {
        const fetchMock = vi.fn().mockResolvedValue(response(policy(0)));
        vi.stubGlobal('fetch', fetchMock);
        render(<AutomationPolicySettings />);
        const limit = await screen.findByLabelText('pages.aiAssistant.automationPolicyMaxTurns');
        await waitFor(() => expect(limit).toHaveValue(20));
        const save = screen.getByRole('button', { name: 'pages.aiAssistant.goalBudgetSave' });
        for (const value of ['0', '1.5', '51', '']) {
            fireEvent.change(limit, { target: { value } });
            expect(save).toBeDisabled();
        }
        fireEvent.change(limit, { target: { value: '50' } });
        expect(save).toBeEnabled();
        expect(fetchMock).toHaveBeenCalledTimes(1);
    });
});
