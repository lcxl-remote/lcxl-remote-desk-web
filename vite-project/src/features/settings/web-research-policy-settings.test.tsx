import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import WebResearchPolicySettings from './web-research-policy-settings';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const policy = (revision: number, maxFetchBytes = 128 * 1024) => ({ schemaVersion: 1, revision, maxFetchBytes });
const response = (data: unknown) => ({ ok: true, json: async () => ({ success: true, data }) });
afterEach(() => vi.unstubAllGlobals());

describe('web research policy settings', () => {
    it('saves the read limit in bytes with the loaded revision', async () => {
        const fetchMock = vi.fn().mockResolvedValueOnce(response(policy(3)))
            .mockResolvedValueOnce(response(policy(4, 512 * 1024)));
        vi.stubGlobal('fetch', fetchMock);
        render(<WebResearchPolicySettings />);
        const limit = await screen.findByLabelText('pages.aiAssistant.webResearchPolicyMaxFetch');
        await waitFor(() => expect(limit).toHaveValue(128));
        fireEvent.change(limit, { target: { value: '512' } });
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAssistant.goalBudgetSave' }));
        await screen.findByText('pages.aiAssistant.webResearchPolicySaved');
        expect(fetchMock.mock.calls[1][0]).toBe('/api/admin/system/web-research-policy');
        expect(JSON.parse(fetchMock.mock.calls[1][1].body)).toEqual({ expectedRevision: 3, maxFetchBytes: 512 * 1024 });
        expect(limit).toHaveValue(512);
    });

    it('rejects values outside 16 to 1024 KiB without submitting', async () => {
        const fetchMock = vi.fn().mockResolvedValue(response(policy(0)));
        vi.stubGlobal('fetch', fetchMock);
        render(<WebResearchPolicySettings />);
        const limit = await screen.findByLabelText('pages.aiAssistant.webResearchPolicyMaxFetch');
        await waitFor(() => expect(limit).toHaveValue(128));
        const save = screen.getByRole('button', { name: 'pages.aiAssistant.goalBudgetSave' });
        for (const value of ['15', '1.5', '1025', '']) {
            fireEvent.change(limit, { target: { value } });
            expect(save).toBeDisabled();
        }
        fireEvent.change(limit, { target: { value: '1024' } });
        expect(save).toBeEnabled();
        expect(fetchMock).toHaveBeenCalledTimes(1);
    });
});
