import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
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

it('shows a running spinner and confirms deletion before sending the request', async () => {
    const row = { sessionId: 's', conversationId: 'c', firstQuestion: 'Running task', updatedAt: '', active: true };
    const fetcher = vi.fn().mockImplementation(async (_url, options) => ({ ok: true, json: async () =>
        options?.method === 'POST' ? { data: { deleted: true } } : { data: { sessions: [row] } } }));
    vi.stubGlobal('fetch', fetcher);
    const deleted = vi.fn();
    render(<AssistantHistory deskId="d" disabled={false} onSelect={() => true} onDeleted={deleted} />);
    fireEvent.click(screen.getByRole('button', { name: 'pages.deviceAssistant.history.title' }));
    await screen.findByLabelText('pages.deviceAssistant.history.running');
    fireEvent.click(screen.getByRole('button', { name: 'pages.deviceAssistant.history.delete' }));
    const dialog = screen.getByRole('dialog', { name: 'pages.deviceAssistant.history.deleteTitle' });
    expect(within(dialog).getByText('pages.deviceAssistant.history.deleteRunning')).toBeTruthy();
    expect(fetcher).toHaveBeenCalledTimes(1);
    fireEvent.click(within(dialog).getByRole('button', { name: 'pages.deviceAssistant.history.delete' }));
    await waitFor(() => expect(deleted).toHaveBeenCalledWith('c'));
    expect(fetcher).toHaveBeenCalledWith('/api/my/device-assistant-session/delete', expect.objectContaining({
        method: 'POST', body: JSON.stringify({ connection: 'd', session: 's' }),
    }));
});

it('can cancel an idle conversation deletion without sending a mutation', async () => {
    const fetcher = vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: { sessions: [
        { sessionId: 'idle', conversationId: 'idle-client', firstQuestion: 'Idle task', updatedAt: '', active: false },
    ] } }) });
    vi.stubGlobal('fetch', fetcher);
    render(<AssistantHistory deskId="d" disabled={false} onSelect={() => true} />);
    fireEvent.click(screen.getByRole('button', { name: 'pages.deviceAssistant.history.title' }));
    fireEvent.click(await screen.findByRole('button', { name: 'pages.deviceAssistant.history.delete' }));
    expect(screen.queryByText('pages.deviceAssistant.history.deleteRunning')).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: 'pages.deviceAssistant.history.keep' }));
    expect(fetcher).toHaveBeenCalledTimes(1);
    expect(screen.getByText('Idle task')).toBeTruthy();
});

it('keeps the confirmation visible when deletion fails', async () => {
    vi.stubGlobal('fetch', vi.fn().mockImplementation(async (_url, options) => options?.method === 'POST'
        ? { ok: false } : { ok: true, json: async () => ({ data: { sessions: [
            { sessionId: 's', conversationId: 'c', firstQuestion: 'Task', updatedAt: '' },
        ] } }) }));
    const deleted = vi.fn();
    render(<AssistantHistory deskId="d" disabled={false} onSelect={() => true} onDeleted={deleted} />);
    fireEvent.click(screen.getByRole('button', { name: 'pages.deviceAssistant.history.title' }));
    fireEvent.click(await screen.findByRole('button', { name: 'pages.deviceAssistant.history.delete' }));
    const dialog = screen.getByRole('dialog', { name: 'pages.deviceAssistant.history.deleteTitle' });
    fireEvent.click(within(dialog).getByRole('button', { name: 'pages.deviceAssistant.history.delete' }));
    await screen.findByText('pages.deviceAssistant.history.deleteError');
    expect(deleted).not.toHaveBeenCalled();
    expect(dialog).toBeTruthy();
});

it('orders running conversations first and each group by last activity', async () => {
    const sessions = [
        { sessionId: '1', conversationId: '1', firstQuestion: 'Idle newer', updatedAt: '2026-09-09T12:00:00Z', active: false },
        { sessionId: '2', conversationId: '2', firstQuestion: 'Running older', updatedAt: '2026-09-09T08:00:00Z', active: true },
        { sessionId: '3', conversationId: '3', firstQuestion: 'Idle older', updatedAt: '2026-09-09T09:00:00Z', active: false },
        { sessionId: '4', conversationId: '4', firstQuestion: 'Running newer', updatedAt: '2026-09-09T10:00:00Z', active: true },
    ];
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: { sessions } }) }));
    render(<AssistantHistory deskId="d" disabled={false} onSelect={() => true} />);
    fireEvent.click(screen.getByRole('button', { name: 'pages.deviceAssistant.history.title' }));
    await screen.findByText('Running newer');
    expect(screen.getAllByRole('button').filter(button => /^(Idle|Running) /.test(button.textContent ?? '')).map(button => button.querySelector('span')?.textContent))
        .toEqual(['Running newer', 'Running older', 'Idle newer', 'Idle older']);
});
