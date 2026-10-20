import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import GoalBudgetPolicySettings from './goal-budget-policy-settings';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

const policy = (revision: number, deviceUnavailableMaxMs = 86_400_000) => ({
    schemaVersion: 1,
    revision,
    limits: {
        activeTimeMs: 7_200_000, deadlineMs: null, modelTokens: 10_000_000, modelCalls: 160,
        toolCalls: 200, slices: 20, stalledSlices: 3,
    },
    deviceUnavailableMaxMs,
});

afterEach(() => vi.unstubAllGlobals());

describe('goal budget policy settings', () => {
    it('keeps both time limits unlimited until their switches are explicitly enabled', async () => {
        const unlimited = { ...policy(1), limits: { ...policy(1).limits, activeTimeMs: null, deadlineMs: null } };
        const fetchMock = vi.fn().mockResolvedValueOnce({ ok: true, json: async () => ({ success: true, data: unlimited }) })
            .mockResolvedValueOnce({ ok: true, json: async () => ({ success: true, data: { ...unlimited, revision: 2, limits: { ...unlimited.limits, activeTimeMs: 7_200_000 } } }) });
        vi.stubGlobal('fetch', fetchMock);
        render(<GoalBudgetPolicySettings />);
        const active = await screen.findByRole('switch', { name: 'pages.aiAssistant.goalBudgetActiveHours' });
        const deadline = screen.getByRole('switch', { name: 'pages.aiAssistant.goalBudgetDeadlineDays' });
        expect(active).not.toBeChecked();
        expect(deadline).not.toBeChecked();
        fireEvent.click(active);
        expect(active).toBeChecked();
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAssistant.goalBudgetSave' }));
        await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
        const saved = JSON.parse(fetchMock.mock.calls[1][1].body);
        expect(saved.limits).toMatchObject({ activeTimeMs: 7_200_000, deadlineMs: null });
    });

    it('edits the always-on device unavailable maximum together with the limits', async () => {
        const fetchMock = vi.fn()
            .mockResolvedValueOnce({ ok: true, json: async () => ({ success: true, data: policy(4) }) })
            .mockResolvedValueOnce({ ok: true, json: async () => ({ success: true, data: policy(5, 7_200_000) }) });
        vi.stubGlobal('fetch', fetchMock);
        render(<GoalBudgetPolicySettings />);
        const hours = await screen.findByLabelText('pages.aiAssistant.goalBudgetDeviceUnavailableHours');
        expect(hours).toHaveValue(24);
        const save = screen.getByRole('button', { name: 'pages.aiAssistant.goalBudgetSave' });
        fireEvent.change(hours, { target: { value: '0' } });
        expect(save).toBeDisabled();
        fireEvent.change(hours, { target: { value: '721' } });
        expect(save).toBeDisabled();
        fireEvent.change(hours, { target: { value: '2' } });
        expect(save).toBeEnabled();
        fireEvent.click(save);
        await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
        const body = JSON.parse(fetchMock.mock.calls[1][1].body);
        expect(body).toMatchObject({ expectedRevision: 4, deviceUnavailableMaxMs: 7_200_000 });
        expect(body.limits.deadlineMs).toBeNull();
        await screen.findByText('pages.aiAssistant.goalBudgetSaved');
    });

    it('rejects a stored policy without a valid device unavailable maximum', async () => {
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({
            ok: true, json: async () => ({ success: true, data: { ...policy(1), deviceUnavailableMaxMs: 0 } }),
        }));
        render(<GoalBudgetPolicySettings />);
        expect(await screen.findByRole('alert')).toBeInTheDocument();
    });
});
