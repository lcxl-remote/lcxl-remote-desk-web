import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { MemoryRouter, Routes, Route, useNavigate } from 'react-router-dom';
import { afterEach, expect, it, vi } from 'vitest';
import AssistantHistorySearch from './assistant-history-search';
import { selectOption } from '@/test-utils/select-option';
vi.mock('./assistant-retained-conversations', () => ({ AssistantRetainedConversations: () => null }));
vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(module => module.reactI18nextMock()));
const inputPolicy = vi.hoisted(() => ({ queryChars: 256, windowDays: 90 }));
vi.mock('./assistant-data-api', async importOriginal => ({
    ...await importOriginal<typeof import('./assistant-data-api')>(),
    getAssistantDataSettings: vi.fn(async () => ({ policy: { limits: { retrieval: { query_chars: inputPolicy.queryChars, search_window_days: inputPolicy.windowDays } } } })),
}));
const ready = () => waitFor(() => expect(screen.getByRole('button', { name: 'Search' })).toBeEnabled());
const fragment = (id: string, text: string) => ({ source: { source_id: id, revision: 1, availability: 'available',
    locator: { conversation_id: 'root', root_conversation_id: 'root' }, content: text,
    recorded_at_unix_ms: 1000, statement_timezone: 'UTC', source_device: { current_name: 'Offline device' } }, role: 'user', kind: 'input' });
const response = (data: unknown) => ({ ok: true, json: async () => ({ success: true, code: 0, data }) });
afterEach(() => { vi.unstubAllGlobals(); inputPolicy.queryChars = 256; inputPolicy.windowDays = 90; });
it('sends keywords in a POST body and opens retained sources without signaling or a live connection', async () => {
    const fetchMock = vi.fn().mockResolvedValue(response({ fragments: [fragment('src_one', 'Sent the report')], next_cursor: null,
        observed_through_generation: 4, partial: true, reasons: ['document_work_limit'] }));
    vi.stubGlobal('fetch', fetchMock);
    render(<MemoryRouter><AssistantHistorySearch /></MemoryRouter>);
    fireEvent.change(screen.getByRole('textbox'), { target: { value: 'private report' } });
    await selectOption(screen.getByRole('combobox', { name: 'Time range' }), 'Last 30 days');
    await selectOption(screen.getByRole('combobox', { name: 'Content type' }), 'Native receipts');
    expect(fetchMock).not.toHaveBeenCalled();
    await ready(); fireEvent.click(screen.getByRole('button', { name: 'Search' }));
    await screen.findByText('Sent the report');
    const [path, request] = fetchMock.mock.calls[0];
    expect(path).toBe('/api/my/ai-assistant-history/search');
    expect(request.method).toBe('POST'); expect(JSON.parse(request.body).queries).toEqual(['private report']);
    const query = JSON.parse(request.body);
    expect(query.source_kinds).toEqual(['receipt']);
    expect(query.time_range.until_unix_ms - query.time_range.from_unix_ms).toBe(30 * 86400000);
    expect(screen.getByRole('link', { name: 'Open original conversation' })).toHaveAttribute('href', '/ai-assistant/conversation?source=src_one&revision=1');
    expect(screen.getByRole('status')).toHaveTextContent('Showing partial history results.');
});
it('keeps the original time scope for pagination and clears results after a business error', async () => {
    const fetchMock = vi.fn().mockResolvedValueOnce(response({ fragments: [fragment('src_one', 'First result')], next_cursor: 'oh1_next',
        observed_through_generation: 4, partial: false, reasons: [] }))
        .mockResolvedValueOnce({ ok: true, json: async () => ({ success: false, code: 8 }) });
    vi.stubGlobal('fetch', fetchMock);
    render(<MemoryRouter><AssistantHistorySearch /></MemoryRouter>);
    await ready(); fireEvent.click(screen.getByRole('button', { name: 'Search' }));
    await screen.findByText('First result');
    fireEvent.click(screen.getByRole('button', { name: 'Load more' }));
    await waitFor(() => expect(screen.queryByText('First result')).toBeNull());
    const original = JSON.parse(fetchMock.mock.calls[0][1].body), page = JSON.parse(fetchMock.mock.calls[1][1].body);
    expect(page.time_range).toEqual(original.time_range); expect(page.cursor).toBe('oh1_next');
});

it('restores the original search after source navigation and revalidates results without putting keywords in a URL', async () => {
    const first = { fragments: [fragment('src_one', 'Before navigation')], next_cursor: null, observed_through_generation: 4, partial: false, reasons: [] };
    const fetchMock = vi.fn().mockResolvedValueOnce(response(first)).mockResolvedValueOnce(response({ ...first, fragments: [fragment('src_one', 'Revalidated result')] }));
    vi.stubGlobal('fetch', fetchMock);
    function Source() { const navigate = useNavigate(); return <button onClick={() => navigate(-1)}>Back to history</button>; }
    render(<MemoryRouter initialEntries={['/ai-assistant/history']}><Routes>
        <Route path="/ai-assistant/history" element={<AssistantHistorySearch />} />
        <Route path="/ai-assistant/conversation" element={<Source />} />
    </Routes></MemoryRouter>);
    fireEvent.change(screen.getByRole('textbox'), { target: { value: 'private report' } });
    await selectOption(screen.getByRole('combobox', { name: 'Time range' }), 'Last 7 days');
    await selectOption(screen.getByRole('combobox', { name: 'Content type' }), 'My messages');
    await ready(); fireEvent.click(screen.getByRole('button', { name: 'Search' }));
    await screen.findByText('Before navigation');
    fireEvent.click(screen.getByRole('link', { name: 'Open original conversation' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Back to history' }));
    await screen.findByText('Revalidated result');
    expect(screen.getByRole('textbox')).toHaveValue('private report');
    expect(screen.getByRole('combobox', { name: 'Time range' })).toHaveTextContent('Last 7 days');
    expect(screen.getByRole('combobox', { name: 'Content type' })).toHaveTextContent('My messages');
    const original = JSON.parse(fetchMock.mock.calls[0][1].body), restored = JSON.parse(fetchMock.mock.calls[1][1].body);
    expect(restored).toEqual(original);
    expect(fetchMock.mock.calls.every(([url]) => !url.includes('private'))).toBe(true);
});

it('counts Unicode characters and uses the configured history window', async () => {
    inputPolicy.queryChars = 2; inputPolicy.windowDays = 3;
    const fetchMock = vi.fn().mockResolvedValue(response({ fragments: [], next_cursor: null,
        observed_through_generation: 4, partial: false, reasons: [] }));
    vi.stubGlobal('fetch', fetchMock); render(<MemoryRouter><AssistantHistorySearch /></MemoryRouter>);
    await ready();
    fireEvent.change(screen.getByRole('textbox'), { target: { value: '中🙂a' } });
    expect(screen.getByRole('button', { name: 'Search' })).toBeDisabled();
    fireEvent.change(screen.getByRole('textbox'), { target: { value: '中🙂' } });
    expect(screen.getByRole('button', { name: 'Search' })).toBeEnabled();
    fireEvent.click(screen.getByRole('button', { name: 'Search' }));
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));
    const query = JSON.parse(fetchMock.mock.calls[0][1].body);
    expect(query.queries).toEqual(['中🙂']);
    expect(query.time_range.until_unix_ms - query.time_range.from_unix_ms).toBe(3 * 86400000);
});
