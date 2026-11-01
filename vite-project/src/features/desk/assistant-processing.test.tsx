import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { afterEach, expect, it, vi } from 'vitest';
import AssistantProcessing from './assistant-processing';
vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(module => module.reactI18nextMock()));
const status = { observed_at_unix_ms: 100, history_retention_days: 90, history_generation: 1, stored_history_fragments: 0, indexed_terms: 0,
    coverage: [], partial: false };
const response = (data: unknown) => ({ ok: true, json: async () => ({ success: true, code: 0, data }) });
afterEach(() => vi.unstubAllGlobals());
it('reads processing metadata without context-recovery fields and refreshes it', async () => {
    const fetchMock = vi.fn().mockResolvedValue(response(status));
    vi.stubGlobal('fetch', fetchMock); render(<MemoryRouter><AssistantProcessing /></MemoryRouter>);
    const refresh = screen.getByRole('button', { name: 'Refresh' });
    await waitFor(() => expect(refresh).not.toBeDisabled());
    expect(screen.queryByRole('button', { name: 'Rebuild context' })).toBeNull();
    expect(screen.queryByRole('alert')).toBeNull();
    fireEvent.click(refresh);
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
});
it('shows business failures and clears the previous status', async () => {
    const fetchMock = vi.fn().mockResolvedValueOnce(response(status))
        .mockResolvedValueOnce({ ok: true, json: async () => ({ success: false, code: 8 }) });
    vi.stubGlobal('fetch', fetchMock); render(<MemoryRouter><AssistantProcessing /></MemoryRouter>);
    const refresh = screen.getByRole('button', { name: 'Refresh' });
    await waitFor(() => expect(refresh).not.toBeDisabled());
    fireEvent.click(refresh);
    expect(await screen.findByRole('alert')).toBeVisible();
});

