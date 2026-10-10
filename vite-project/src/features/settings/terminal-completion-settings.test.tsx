import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { TerminalCompletionSettings } from './terminal-completion-settings';

const api = vi.hoisted(() => ({ getTerminalCompletion: vi.fn(), updateTerminalCompletion: vi.fn() }));
vi.mock('@/services/clients', () => api);
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
beforeEach(() => {
    vi.resetAllMocks();
    api.getTerminalCompletion.mockResolvedValue({ success: true, data: { revision: 0, maxOutputTokens: 512 } });
});

describe('terminal completion settings', () => {
    it('saves the draft with its revision and reloads the committed value', async () => {
        render(<TerminalCompletionSettings />);
        const input = await screen.findByDisplayValue('512');
        fireEvent.change(input, { target: { value: '32768' } });
        api.updateTerminalCompletion.mockResolvedValue({ success: true, data: { revision: 1, maxOutputTokens: 32768 } });
        api.getTerminalCompletion.mockResolvedValue({ success: true, data: { revision: 1, maxOutputTokens: 32768 } });
        fireEvent.click(screen.getByRole('button'));
        await waitFor(() => expect(api.updateTerminalCompletion).toHaveBeenCalledWith({ expectedRevision: 0, maxOutputTokens: 32768 }));
        expect(await screen.findByRole('status')).toHaveTextContent('pages.terminalCompletion.saved');
        expect(api.getTerminalCompletion).toHaveBeenCalledTimes(2);
    });
    it('blocks invalid values and reloads a concurrent update after a conflict', async () => {
        render(<TerminalCompletionSettings />);
        const input = await screen.findByDisplayValue('512');
        fireEvent.change(input, { target: { value: '1.5' } });
        expect(screen.getByRole('button')).toBeDisabled();
        expect(api.updateTerminalCompletion).not.toHaveBeenCalled();
        fireEvent.change(input, { target: { value: '1024' } });
        api.updateTerminalCompletion.mockResolvedValue({ success: false });
        api.getTerminalCompletion.mockResolvedValue({ success: true, data: { revision: 2, maxOutputTokens: 2048 } });
        fireEvent.click(screen.getByRole('button'));
        expect(await screen.findByRole('alert')).toHaveTextContent('pages.terminalCompletion.error');
        expect(await screen.findByDisplayValue('2048')).toBeTruthy();
        expect(screen.queryByRole('status')).toBeNull();
    });
});
