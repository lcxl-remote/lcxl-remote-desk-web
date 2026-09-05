import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { AssistantHistory } from './assistant-history';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
afterEach(() => vi.unstubAllGlobals());
describe('assistant history', () => {
    it('loads device history on demand and resumes the chosen conversation', async () => {
        const fetcher = vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: { sessions: [
            { sessionId: 's1', conversationId: 'c1', firstQuestion: 'Old question', updatedAt: '2026-09-05' },
        ] } }) });
        vi.stubGlobal('fetch', fetcher);
        const select = vi.fn().mockReturnValue(true);
        render(<AssistantHistory deskId="device/1" disabled={false} onSelect={select} />);
        expect(fetcher).not.toHaveBeenCalled();
        fireEvent.click(screen.getByRole('button'));
        fireEvent.click(await screen.findByText('Old question'));
        expect(fetcher.mock.calls[0][0]).toContain('connection=device%2F1');
        expect(select).toHaveBeenCalledWith('c1');
        await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    });

    it('shows errors instead of an empty list and allows retry', async () => {
        vi.stubGlobal('fetch', vi.fn().mockRejectedValueOnce(new Error()).mockResolvedValue({
            ok: true, json: async () => ({ data: { sessions: [] } }),
        }));
        render(<AssistantHistory deskId="d" disabled onSelect={() => false} />);
        fireEvent.click(screen.getByRole('button'));
        expect(await screen.findByRole('alert')).toBeTruthy();
        expect(screen.queryByText('pages.deviceAssistant.history.empty')).toBeNull();
        fireEvent.click(screen.getByText('pages.deviceAssistant.history.retry'));
        expect(await screen.findByText('pages.deviceAssistant.history.empty')).toBeTruthy();
    });

    it('does not switch away from an active task', async () => {
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: { sessions: [
            { sessionId: 's', conversationId: 'c', firstQuestion: 'Previous task', updatedAt: '' },
        ] } }) }));
        const select = vi.fn();
        render(<AssistantHistory deskId="d" disabled onSelect={select} />);
        fireEvent.click(screen.getByRole('button'));
        fireEvent.click(await screen.findByText('Previous task'));
        expect(select).not.toHaveBeenCalled();
    });
});
