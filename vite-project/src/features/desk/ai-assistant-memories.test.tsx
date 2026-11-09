import { assistantSourceId } from '@/test/assistant-source-id';
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { afterEach, expect, it, vi } from 'vitest';
import { deskErrorCodeEnum } from '@/services/types';
import AiAssistantMemories from './ai-assistant-memories';
import { selectOption } from '@/test-utils/select-option';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(module => module.reactI18nextMock()));

const memoryId = 'mem_AAECAwQFBgcICQoLDA0ODw';
const sourceId = 'src_h42i';
const otherId = 'mem_AAAAAAAAAAAAAAAAAAAAAA';
const memory = { memory_id: memoryId, revision: 7, scope: 'personal', source_device: null,
    content: 'Prefer Chinese',
    context: 'For technical discussions', status: 'active', source_ids: [sourceId],
    observed_at_unix_ms: 100, updated_at_unix_ms: 200, valid_from_unix_ms: 100, valid_until_unix_ms: null };
const page = (memories = [memory], next: string | null = null) => ({ memories, next_cursor: next,
    memory_state_generation: 17, control_generation: 8 });
const response = (data: unknown) => ({ ok: true, json: async () => ({ success: true, code: 0, data }) });
const receipt = (operationId: string, outcome = 'applied', reason = 'memory_forget') => response({ operation_id: operationId, outcome,
    reason, affected_count: 1, memory_state_generation: 18, control_generation: 9, cleanup_pending: true });
const renderPage = () => render(<MemoryRouter><AiAssistantMemories /></MemoryRouter>);
afterEach(() => vi.unstubAllGlobals());

it('lists saved facts and sources without requiring automatic memory to be enabled or offering manual saves', async () => {
    const fetchMock = vi.fn().mockResolvedValue(response(page()));
    vi.stubGlobal('fetch', fetchMock);
    renderPage();
    await screen.findByText('Prefer Chinese');
    expect(screen.getByText('For technical discussions')).toBeVisible();
    expect(screen.getByRole('button', { name: 'View source 1' })).toBeEnabled();
    expect(screen.getByRole('button', { name: 'Delete memory' })).toBeEnabled();
    expect(screen.queryByRole('textbox')).toBeNull();
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(fetchMock.mock.calls[0][0]).toMatch(/^\/api\/my\/ai-assistant-memories\?/);
    expect(fetchMock.mock.calls[0][1]).toMatchObject({ credentials: 'include', cache: 'no-store' });
    for (const [label, scope] of [['Personal', 'personal'], ['Device', 'device'], ['All', null]] as const) {
        await selectOption(screen.getByRole('combobox', { name: 'Scope' }), label);
        await waitFor(() => expect(new URL(fetchMock.mock.calls.at(-1)![0], 'https://example.test').searchParams.get('scope')).toBe(scope));
        expect(screen.getByRole('combobox', { name: 'Scope' })).toHaveTextContent(label);
    }
});

it('shows independently retained source text as plain text and clears it when closed', async () => {
    const fetchMock = vi.fn().mockResolvedValueOnce(response(page())).mockResolvedValueOnce(response({
        source_id: sourceId, revision: 7, availability: 'evidence_only', locator: null, source_device: null,
        content: '<script>untrusted source</script>', recorded_at_unix_ms: 100, statement_timezone: 'Asia/Shanghai', notice: null,
    }));
    vi.stubGlobal('fetch', fetchMock);
    renderPage();
    fireEvent.click(await screen.findByRole('button', { name: 'View source 1' }));
    expect(await screen.findByText('<script>untrusted source</script>')).toBeVisible();
    expect(screen.getByText(/Independently retained evidence/)).toBeVisible();
    expect(screen.getByText('Original statement time zone: Asia/Shanghai')).toBeVisible();
    expect(document.querySelector('script')).toBeNull();
    expect(fetchMock.mock.calls[1][0]).toContain(encodeURIComponent(sourceId));
    fireEvent.click(screen.getByRole('button', { name: 'Close' }));
    await waitFor(() => expect(screen.queryByText('<script>untrusted source</script>')).toBeNull());
});

it('confirms the visible revision and reuses the operation identity after an uncertain network failure', async () => {
    let posts = 0;
    const fetchMock = vi.fn(async (_url: string, options: RequestInit) => {
        if (options.method !== 'POST') return response(posts > 1 ? page([]) : page());
        posts += 1;
        if (posts === 1) throw new Error('Connection lost after server commit');
        return receipt(JSON.parse(options.body as string).operation_id, 'already_applied');
    });
    vi.stubGlobal('fetch', fetchMock);
    renderPage();
    fireEvent.click(await screen.findByRole('button', { name: 'Delete memory' }));
    const dialog = screen.getByRole('alertdialog');
    expect(within(dialog).getByText('Prefer Chinese')).toBeVisible();
    expect(fetchMock).toHaveBeenCalledTimes(1);
    fireEvent.click(within(dialog).getByRole('button', { name: 'Delete memory' }));
    await within(dialog).findByRole('alert');
    const first = JSON.parse(fetchMock.mock.calls[1][1].body as string);
    expect(first).toMatchObject({ memory_id: memoryId, expected_revision: 7 });
    fireEvent.click(within(dialog).getByRole('button', { name: 'Delete memory' }));
    await screen.findByText('The long-term memory was deleted.');
    const second = JSON.parse(fetchMock.mock.calls[2][1].body as string);
    expect(second).toEqual(first);
    await screen.findByText('There are no viewable long-term memories in this scope.');
});

it('requires an explicit clear confirmation and sends the collection generation rather than the visible ids', async () => {
    const fetchMock = vi.fn(async (_url: string, options: RequestInit) => {
        if (options.method !== 'POST') return response(page());
        return receipt(JSON.parse(options.body as string).operation_id, 'applied', 'memory_clear');
    });
    vi.stubGlobal('fetch', fetchMock);
    renderPage();
    await screen.findByText('Prefer Chinese');
    fireEvent.click(screen.getByRole('button', { name: 'Clear all memories' }));
    expect(fetchMock).toHaveBeenCalledTimes(1);
    const dialog = screen.getByRole('alertdialog');
    expect(within(dialog).getByText(/Clear all personal and device memories/)).toBeVisible();
    fireEvent.click(within(dialog).getByRole('button', { name: 'Clear all memories' }));
    await screen.findByText('Long-term memories were cleared.');
    const request = JSON.parse(fetchMock.mock.calls[1][1].body as string);
    expect(request.expected_memory_state_generation).toBe(17);
    expect(Object.keys(request).sort()).toEqual(['expected_memory_state_generation', 'operation_id']);
    expect(fetchMock.mock.calls[1][0]).toBe('/api/my/ai-assistant-memory/clear');
});

it('does not claim a stale control succeeded and refreshes the changed memory', async () => {
    let posts = 0;
    const fetchMock = vi.fn(async (_url: string, options: RequestInit) => {
        if (options.method !== 'POST') return response(page(posts ? [{ ...memory, revision: 8, content: 'Prefer English' }] : [memory]));
        posts += 1;
        return receipt(JSON.parse(options.body as string).operation_id, 'stale_target');
    });
    vi.stubGlobal('fetch', fetchMock);
    renderPage();
    fireEvent.click(await screen.findByRole('button', { name: 'Delete memory' }));
    fireEvent.click(within(screen.getByRole('alertdialog')).getByRole('button', { name: 'Delete memory' }));
    await screen.findByText('Prefer English');
    expect(screen.queryByText('The long-term memory was deleted.')).toBeNull();
    expect(screen.getByText(/Memories changed in another conversation/)).toBeVisible();
});

it('binds pagination to the original generations and drops stale rows on a conflict', async () => {
    const fetchMock = vi.fn().mockResolvedValueOnce(response(page([memory], otherId)))
        .mockResolvedValueOnce({ ok: true, json: async () => ({ success: false, code: deskErrorCodeEnum.REVISION_CONFLICT }) });
    vi.stubGlobal('fetch', fetchMock);
    renderPage();
    fireEvent.click(await screen.findByRole('button', { name: 'Load more' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Memories changed in another conversation');
    expect(screen.queryByText('Prefer Chinese')).toBeNull();
    const query = new URL(fetchMock.mock.calls[1][0], 'https://example.test').searchParams;
    expect(query.get('cursor')).toBe(otherId);
    expect(query.get('memory_state_generation')).toBe('17');
    expect(query.get('control_generation')).toBe('8');
});

it('ignores a late source response after the owner closes the source view', async () => {
    let finish!: (value: ReturnType<typeof response>) => void;
    const pending = new Promise<ReturnType<typeof response>>(resolve => { finish = resolve; });
    const fetchMock = vi.fn().mockResolvedValueOnce(response(page())).mockReturnValueOnce(pending);
    vi.stubGlobal('fetch', fetchMock);
    renderPage();
    fireEvent.click(await screen.findByRole('button', { name: 'View source 1' }));
    fireEvent.click(screen.getByRole('button', { name: 'Close' }));
    await act(async () => { finish(response({ source_id: sourceId, revision: 7, availability: 'evidence_only',
        content: 'Late original evidence', locator: null, source_device: null, recorded_at_unix_ms: 100, statement_timezone: 'Asia/Shanghai', notice: null })); });
    expect(screen.queryByText('Late original evidence')).toBeNull();
});

it('rejects an apparently successful page if its frozen generations changed', async () => {
    const fetchMock = vi.fn().mockResolvedValueOnce(response(page([memory], otherId)))
        .mockResolvedValueOnce(response({ ...page([{ ...memory, memory_id: otherId, content: 'Unfenced new row' }]),
            memory_state_generation: 18 }));
    vi.stubGlobal('fetch', fetchMock);
    renderPage();
    fireEvent.click(await screen.findByRole('button', { name: 'Load more' }));
    await screen.findByRole('alert');
    expect(screen.queryByText('Unfenced new row')).toBeNull();
});

it('does not display original text with an unknown statement time zone', async () => {
    const fetchMock = vi.fn().mockResolvedValueOnce(response(page())).mockResolvedValueOnce(response({
        source_id: sourceId, revision: 7, availability: 'evidence_only', locator: null, source_device: null,
        content: 'Yesterday in an unknown zone', recorded_at_unix_ms: 100, statement_timezone: 'Unknown/Timezone', notice: null,
    }));
    vi.stubGlobal('fetch', fetchMock);
    renderPage();
    fireEvent.click(await screen.findByRole('button', { name: 'View source 1' }));
    await within(screen.getByRole('alertdialog')).findByRole('alert');
    expect(screen.queryByText('Yesterday in an unknown zone')).toBeNull();
});

it('keeps a clear request unconfirmed if the receipt has the wrong control reason', async () => {
    const fetchMock = vi.fn(async (_url: string, options: RequestInit) => {
        if (options.method !== 'POST') return response(page());
        return receipt(JSON.parse(options.body as string).operation_id, 'applied', 'memory_forget');
    });
    vi.stubGlobal('fetch', fetchMock);
    renderPage();
    await screen.findByText('Prefer Chinese');
    fireEvent.click(screen.getByRole('button', { name: 'Clear all memories' }));
    fireEvent.click(within(screen.getByRole('alertdialog')).getByRole('button', { name: 'Clear all memories' }));
    await within(screen.getByRole('alertdialog')).findByRole('alert');
    expect(screen.queryByText('Long-term memories were cleared.')).toBeNull();
    expect(screen.getAllByText('Prefer Chinese').length).toBeGreaterThan(0);
});

it('shows saved content and sources beyond the previous fixed capacities', async () => {
    const large = { ...memory, content: '中'.repeat(3000), source_ids: Array.from({ length: 9 }, (_, i) => assistantSourceId(i + 1)) };
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response(page([large]))));
    render(<MemoryRouter><AiAssistantMemories /></MemoryRouter>);
    expect(await screen.findByText(large.content)).toBeVisible();
});
