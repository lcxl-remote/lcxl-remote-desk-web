import { beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { ComputerUseApplicationPolicySettings } from './computer-use-application-policy';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then((m) => m.reactI18nextMock()));
const api = vi.hoisted(() => ({ load: vi.fn(), save: vi.fn() }));
vi.mock('@/services/clients', () => ({
    queryComputerUseApplicationPolicy: api.load,
    updateComputerUseApplicationPolicy: api.save,
}));

beforeEach(() => {
    vi.clearAllMocks();
    api.load.mockResolvedValue({ data: { revision: 7, allowed_application_paths: ['/Applications/Test.app/Contents/MacOS/Test'] } });
    api.save.mockResolvedValue({ data: { revision: 8, allowed_application_paths: [] } });
});

async function openPolicy() {
    render(<ComputerUseApplicationPolicySettings />);
    fireEvent.click(screen.getByText('Advanced: configure application scope'));
    expect(api.load).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: 'Read local policy' }));
    await screen.findByRole('combobox', { name: 'Application scope' });
}

describe('local application policy', () => {
    it('preserves an existing restriction until explicitly removed with its observed revision', async () => {
        await openPolicy();
        expect(screen.getByRole('combobox')).toHaveValue('restricted');
        expect(screen.getByRole('textbox')).toHaveValue('/Applications/Test.app/Contents/MacOS/Test');
        fireEvent.change(screen.getByRole('combobox'), { target: { value: 'unrestricted' } });
        fireEvent.click(screen.getByRole('button', { name: 'Save application policy' }));
        await waitFor(() => expect(api.save).toHaveBeenCalledWith({ expected_revision: 7, allowed_application_paths: [] }));
        await screen.findByText('Application policy saved and applied.');
    });
    it('does not turn an empty restricted form into an unrestricted save', async () => {
        await openPolicy();
        fireEvent.change(screen.getByRole('textbox'), { target: { value: '' } });
        expect(screen.getByRole('button', { name: 'Save application policy' })).toBeDisabled();
        expect(api.save).not.toHaveBeenCalled();
    });
    it('requires reloading after a conflict or worker failure, without retrying a stale edit', async () => {
        api.save.mockRejectedValueOnce(new Error('conflict'));
        await openPolicy();
        fireEvent.click(screen.getByRole('button', { name: 'Save application policy' }));
        await screen.findByRole('status');
        expect(screen.queryByRole('combobox')).not.toBeInTheDocument();
        expect(api.save).toHaveBeenCalledTimes(1);
        expect(api.load).toHaveBeenCalledTimes(1);
    });
    it('does not expose an editable default when loading fails', async () => {
        api.load.mockRejectedValueOnce(new Error('permission denied'));
        render(<ComputerUseApplicationPolicySettings />);
        fireEvent.click(screen.getByText('Advanced: configure application scope'));
        fireEvent.click(screen.getByRole('button', { name: 'Read local policy' }));
        await screen.findByRole('status');
        expect(screen.queryByRole('combobox')).not.toBeInTheDocument();
    });
});
