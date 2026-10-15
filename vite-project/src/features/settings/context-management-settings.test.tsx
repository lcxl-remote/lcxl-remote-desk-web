import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ContextManagementSettings } from './context-management-settings';

const api = vi.hoisted(() => ({ getContextManagement: vi.fn(), updateContextManagement: vi.fn() }));
vi.mock('@/services/clients', () => api);
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
beforeEach(() => { vi.resetAllMocks(); });

describe('context management settings', () => {
    it('loads the default and saves an explicit window choice with the revision', async () => {
        api.getContextManagement.mockResolvedValue({ success: true, data: { revision: 0, strategy: 'checkpoint_summary', summaryMaxOutputTokens: 4096, compressionHistoryTargetPercent: 50 } });
        api.updateContextManagement.mockResolvedValue({ success: true, data: { revision: 1, strategy: 'window', summaryMaxOutputTokens: 4096, compressionHistoryTargetPercent: 50 } });
        render(<ContextManagementSettings />);
        await waitFor(() => expect(screen.getByRole('switch')).not.toBeDisabled());
        expect(screen.getByRole('switch')).toHaveAttribute('aria-checked', 'true');
        fireEvent.click(screen.getByRole('switch'));
        fireEvent.click(screen.getByRole('button', { name: 'pages.contextManagement.save' }));
        await waitFor(() => expect(api.updateContextManagement).toHaveBeenCalledWith({ expectedRevision: 0, strategy: 'window', summaryMaxOutputTokens: 4096, compressionHistoryTargetPercent: 50 }));
        expect(await screen.findByRole('status')).toHaveTextContent('pages.contextManagement.saved');
    });

    it('does not pretend success on a conflict and reloads the current setting', async () => {
        api.getContextManagement.mockResolvedValueOnce({ success: true, data: { revision: 0, strategy: 'checkpoint_summary', summaryMaxOutputTokens: 4096, compressionHistoryTargetPercent: 50 } })
            .mockResolvedValueOnce({ success: true, data: { revision: 4, strategy: 'checkpoint_summary', summaryMaxOutputTokens: 4096, compressionHistoryTargetPercent: 50 } });
        api.updateContextManagement.mockResolvedValue({ success: false });
        render(<ContextManagementSettings />);
        await waitFor(() => expect(screen.getByRole('switch')).not.toBeDisabled());
        fireEvent.click(screen.getByRole('switch'));
        fireEvent.click(screen.getByRole('button', { name: 'pages.contextManagement.save' }));
        expect(await screen.findByRole('alert')).toBeTruthy();
        expect(screen.queryByRole('status')).toBeNull();
        expect(screen.getByRole('switch')).toHaveAttribute('aria-checked', 'true');
    });
    it('saves a history target and rejects goals outside the final contract', async () => {
        api.getContextManagement.mockResolvedValue({ success: true, data: { revision: 0, strategy: 'checkpoint_summary', summaryMaxOutputTokens: 16384, compressionHistoryTargetPercent: 50 } });
        api.updateContextManagement.mockResolvedValue({ success: true, data: { revision: 1, strategy: 'checkpoint_summary', summaryMaxOutputTokens: 16384, compressionHistoryTargetPercent: 75 } });
        render(<ContextManagementSettings />);
        const target = await screen.findByRole('spinbutton', { name: 'pages.contextManagement.historyTarget' });
        await waitFor(() => expect(target).toHaveValue(50));
        for (const value of ['0', '100', '1.5']) {
            fireEvent.change(target, { target: { value } });
            expect(screen.getByRole('button', { name: 'pages.contextManagement.save' })).toBeDisabled();
        }
        fireEvent.change(target, { target: { value: '75' } });
        fireEvent.click(screen.getByRole('button', { name: 'pages.contextManagement.save' }));
        await waitFor(() => expect(api.updateContextManagement).toHaveBeenCalledWith({ expectedRevision: 0, strategy: 'checkpoint_summary', summaryMaxOutputTokens: 16384, compressionHistoryTargetPercent: 75 }));
    });

});
