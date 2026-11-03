import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { AccessCodeProtectionSettingsCard } from './access-code-protection-settings';
vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
const api = vi.hoisted(() => ({ getAccessCodeProtection: vi.fn(), updateAccessCodeProtection: vi.fn() }));
vi.mock('@/services/clients', () => api);
const policy = { request_window_sec: 60, user_max_attempts: null, network_max_attempts: 5, backoff_enabled: true,
    failure_window_sec: 900, max_failures: 20, lock_base_sec: 60, lock_max_sec: 3600 };
beforeEach(() => {
    vi.clearAllMocks();
    api.getAccessCodeProtection.mockResolvedValue({ success: true, data: { revision: 7, policy } });
});
describe('access-code protection settings', () => {
    it('saves an OSS policy with its revision and omits account controls', async () => {
        render(<AccessCodeProtectionSettingsCard />);
        const input = await screen.findByLabelText('Attempts per source network');
        await waitFor(() => expect(input).toHaveValue(5));
        expect(screen.queryByLabelText('Attempts per account')).toBeNull();
        fireEvent.change(input, { target: { value: '3' } });
        api.updateAccessCodeProtection.mockResolvedValue({ success: true, data: { revision: 8, policy: { ...policy, network_max_attempts: 3 } } });
        fireEvent.click(screen.getByRole('button', { name: 'Save' }));
        await waitFor(() => expect(api.updateAccessCodeProtection).toHaveBeenCalledWith({ expected_revision: 7, policy: { ...policy, network_max_attempts: 3 } }));
        expect(await screen.findByRole('status')).toHaveTextContent('Access-code protection settings saved');
        expect(screen.getByRole('button', { name: 'Save' })).toBeDisabled();
    });
    it('rejects fractional, zero and out-of-range values and inconsistent locks', async () => {
        render(<AccessCodeProtectionSettingsCard />);
        const input = await screen.findByLabelText('Attempts per source network');
        await waitFor(() => expect(input).toHaveValue(5));
        for (const value of ['0', '100001', '2.5', '']) {
            fireEvent.change(input, { target: { value } });
            expect(screen.getByRole('button')).toBeDisabled();
        }
        fireEvent.change(input, { target: { value: '3' } });
        fireEvent.change(screen.getByLabelText('Maximum lock (seconds)'), { target: { value: '30' } });
        expect(screen.getByRole('button')).toBeDisabled();
        expect(api.updateAccessCodeProtection).not.toHaveBeenCalled();
    });
    it('uses Manager clients and exposes the account limit', async () => {
        const manager = { ...policy, user_max_attempts: 20, network_max_attempts: 60 };
        const load = vi.fn().mockResolvedValue({ success: true, data: { revision: 9, policy: manager } });
        const save = vi.fn().mockResolvedValue({ success: true, data: { revision: 10, policy: { ...manager, user_max_attempts: 4, backoff_enabled: false } } });
        render(<AccessCodeProtectionSettingsCard loadSettings={load} saveSettings={save} />);
        fireEvent.change(await screen.findByLabelText('Attempts per account'), { target: { value: '4' } });
        fireEvent.click(screen.getByRole('switch'));
        fireEvent.click(screen.getByRole('button'));
        await waitFor(() => expect(save).toHaveBeenCalledWith({ expected_revision: 9, policy: { ...manager, user_max_attempts: 4, backoff_enabled: false } }));
        expect(api.getAccessCodeProtection).not.toHaveBeenCalled();
    });
    it('reloads after a rejected write and never reports it as saved', async () => {
        render(<AccessCodeProtectionSettingsCard />);
        const input = await screen.findByLabelText('Attempts per source network');
        await waitFor(() => expect(input).toHaveValue(5));
        fireEvent.change(input, { target: { value: '3' } });
        api.updateAccessCodeProtection.mockResolvedValue({ success: false });
        api.getAccessCodeProtection.mockResolvedValue({ success: true, data: { revision: 8, policy: { ...policy, network_max_attempts: 9 } } });
        fireEvent.click(screen.getByRole('button'));
        await waitFor(() => expect(input).toHaveValue(9));
        expect(screen.getByRole('alert')).toHaveTextContent('Access-code settings were not saved');
        expect(screen.queryByRole('status')).toBeNull();
    });
});
