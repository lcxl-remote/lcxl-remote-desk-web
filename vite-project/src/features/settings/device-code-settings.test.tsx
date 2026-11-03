import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { DeviceCodeSettingsCard } from './device-code-settings';
vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
const api = vi.hoisted(() => ({ getDeviceCodeSettings: vi.fn(), updateDeviceCodeSettings: vi.fn() }));
vi.mock('@/services/clients', () => api);
beforeEach(() => {
    vi.clearAllMocks();
    api.getDeviceCodeSettings.mockResolvedValue({ success: true, data: { revision: 0, length: 8 } });
});
describe('device code length settings', () => {
    it('saves a valid length with the current revision and adopts the authoritative result', async () => {
        render(<DeviceCodeSettingsCard />);
        const input = await screen.findByDisplayValue('8');
        expect(input).toHaveAccessibleName('New device code length');
        expect(screen.getByText(/Changes apply to first assignments/)).toBeInTheDocument();
        fireEvent.change(input, { target: { value: '12' } });
        api.updateDeviceCodeSettings.mockResolvedValue({ success: true, data: { revision: 1, length: 12 } });
        fireEvent.click(screen.getByRole('button'));
        await waitFor(() => expect(api.updateDeviceCodeSettings).toHaveBeenCalledWith({ expected_revision: 0, length: 12 }));
        expect(await screen.findByRole('status')).toHaveTextContent('Device code settings saved');
        expect(screen.getByRole('button')).toBeDisabled();
    });
    it('rejects fractional and out-of-range lengths and reloads a conflicting write', async () => {
        render(<DeviceCodeSettingsCard />);
        const input = await screen.findByDisplayValue('8');
        for (const value of ['5', '65', '8.5', '']) {
            fireEvent.change(input, { target: { value } });
            expect(screen.getByRole('button')).toBeDisabled();
        }
        expect(api.updateDeviceCodeSettings).not.toHaveBeenCalled();
        fireEvent.change(input, { target: { value: '6' } });
        api.updateDeviceCodeSettings.mockResolvedValue({ success: false, code: 13 });
        api.getDeviceCodeSettings.mockResolvedValue({ success: true, data: { revision: 1, length: 64 } });
        fireEvent.click(screen.getByRole('button'));
        expect(await screen.findByDisplayValue('64')).toBeInTheDocument();
        expect(screen.getByRole('alert')).toHaveTextContent('Device code settings were not saved. Reload and try again.');
        expect(screen.queryByRole('status')).toBeNull();
    });
    it('uses the supplied host client for Manager settings', async () => {
        const load = vi.fn().mockResolvedValue({ success: true, data: { revision: 7, length: 6 } });
        const save = vi.fn().mockResolvedValue({ success: true, data: { revision: 8, length: 8 } });
        render(<DeviceCodeSettingsCard loadSettings={load} saveSettings={save} />);
        fireEvent.change(await screen.findByDisplayValue('6'), { target: { value: '8' } });
        fireEvent.click(screen.getByRole('button'));
        await waitFor(() => expect(save).toHaveBeenCalledWith({ expected_revision: 7, length: 8 }));
        expect(api.getDeviceCodeSettings).not.toHaveBeenCalled();
    });
});
