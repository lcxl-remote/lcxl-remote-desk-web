import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { afterEach, expect, it, vi } from 'vitest';
import { AssistantRetainedConversations } from './assistant-retained-conversations';
vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(module => module.reactI18nextMock()));
const item = { conversation_id: 'root', source: { source_id: 'src_one', revision: 1, availability: 'available', locator: { root_conversation_id: 'root' },
    content: 'Retained original', recorded_at_unix_ms: 1000 }, execution_available: false, active: false };
const response = (data: unknown) => ({ ok: true, json: async () => ({ success: true, code: 0, data }) });
afterEach(() => vi.unstubAllGlobals());
it('lists offline retained roots and deletes through the original endpoint only after confirmation', async () => {
    const fetchMock = vi.fn().mockResolvedValueOnce(response({ conversations: [item], next_cursor: null, partial: false }))
        .mockResolvedValueOnce(response({ deleted: true, backup_cleanup_pending: true }))
        .mockResolvedValueOnce(response({ conversations: [], next_cursor: null, partial: false }));
    vi.stubGlobal('fetch', fetchMock);
    render(<MemoryRouter><AssistantRetainedConversations /></MemoryRouter>);
    await screen.findByText('Retained original');
    expect(screen.getByText(/execution state reclaimed/)).toBeVisible();
    fireEvent.click(screen.getByRole('button', { name: 'Delete conversation' }));
    expect(fetchMock).toHaveBeenCalledTimes(1);
    fireEvent.click(within(screen.getByRole('alertdialog')).getByRole('button', { name: 'Delete conversation' }));
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(3));
    expect(fetchMock.mock.calls[1][0]).toBe('/api/my/ai-assistant-session/delete');
    expect(JSON.parse(fetchMock.mock.calls[1][1].body)).toEqual({ connection: '', session: 'root' });
    await waitFor(() => expect(screen.queryByText('Retained original')).toBeNull());
});
it('clears previous results when a subsequent list request fails', async () => {
    const fetchMock = vi.fn().mockResolvedValueOnce(response({ conversations: [item], next_cursor: null, partial: false }))
        .mockResolvedValueOnce({ ok: true, json: async () => ({ success: false, code: 8 }) });
    vi.stubGlobal('fetch', fetchMock);
    render(<MemoryRouter><AssistantRetainedConversations /></MemoryRouter>);
    await screen.findByText('Retained original');
    fireEvent.click(screen.getByRole('button', { name: 'Refresh' }));
    await waitFor(() => expect(screen.queryByText('Retained original')).toBeNull());
    expect(screen.getByRole('alert')).toBeVisible();
});
