import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { afterEach, expect, it, vi } from 'vitest';
import { deskErrorCodeEnum } from '@/services/types';
import AiAssistantPersonalization from './ai-assistant-personalization';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(module => module.reactI18nextMock()));

const personalPath = '/api/my/ai-assistant-user-instructions';
const settingsPath = '/api/my/ai-assistant-settings';
const personal = { revision: 7, content: 'Start with the conclusion.' };
const settings = { revision: 11, automatic_memory_enabled: true, model_history_search_enabled: true,
    explanation_version: 1 };
const policy = { limits: { memory: { user_instructions_bytes: 8192 }, retrieval: { query_groups: 4, query_chars: 256, search_window_days: 90 } }, revision: 3, history_retention_days: 90 };
const ok = (data: unknown) => ({ ok: true, json: async () => ({ success: true, code: 0, data }) });
const failed = (code: number) => ({ ok: true, json: async () => ({ success: false, code }) });
const open = () => render(<MemoryRouter><AiAssistantPersonalization /></MemoryRouter>);
const ready = async () => {
    await screen.findByDisplayValue(personal.content);
    await screen.findByRole('switch', { name: 'Automatic memory' });

};
afterEach(() => vi.unstubAllGlobals());

it('loads independent personal settings and describes the new-conversation scope', async () => {
    const fetchMock = vi.fn(async (path: string, _options: RequestInit) => ok(path === personalPath ? personal : { settings, policy }));
    vi.stubGlobal('fetch', fetchMock);
    open(); await ready();
    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(screen.queryByLabelText('Daily background memory token limit')).toBeNull();
    expect(screen.getByText('Instructions for new conversations')).toBeVisible();
    expect(screen.queryByLabelText('Recording time zone')).toBeNull();
    expect(screen.getByRole('link', { name: 'Manage long-term memory' })).toHaveAttribute('href', '/ai-assistant/memories');
    expect(screen.getByRole('button', { name: 'Save personal instructions' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Save memory and history settings' })).toBeDisabled();
    for (const [, options] of fetchMock.mock.calls) expect(options).toMatchObject({ credentials: 'include', cache: 'no-store' });
});

it('saves instructions with the displayed revision without changing memory or history settings', async () => {
    const fetchMock = vi.fn(async (path: string, options: RequestInit) => {
        if (options.method !== 'POST') return ok(path === personalPath ? personal : { settings, policy });
        return ok({ revision: 8, content: JSON.parse(options.body as string).content });
    });
    vi.stubGlobal('fetch', fetchMock);
    open(); await ready();
    fireEvent.change(screen.getByLabelText('Personal instructions'), { target: { value: '请用中文，先给结论。' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save personal instructions' }));
    await screen.findByText('Saved.');
    const posts = fetchMock.mock.calls.filter(([, options]) => options.method === 'POST');
    expect(posts).toHaveLength(1);
    expect(posts[0][0]).toBe(personalPath);
    expect(JSON.parse(posts[0][1].body as string)).toEqual({ expected_revision: 7, content: '请用中文，先给结论。' });
    expect(screen.getByRole('switch', { name: 'Automatic memory' })).toBeChecked();
});

it('counts multibyte instructions by UTF-8 bytes and refuses to save oversized content', async () => {
    const fetchMock = vi.fn(async (path: string, _options: RequestInit) => ok(path === personalPath ? personal : { settings, policy }));
    vi.stubGlobal('fetch', fetchMock);
    open(); await ready();
    fireEvent.change(screen.getByLabelText('Personal instructions'), { target: { value: '中'.repeat(3000) } });
    expect(screen.getByText('9000 / 8192 bytes used')).toBeVisible();
    expect(screen.getByRole('button', { name: 'Save personal instructions' })).toBeDisabled();
    expect(fetchMock).toHaveBeenCalledTimes(2);
});

it('turns off automatic memory independently and saves only the remaining settings', async () => {
    const fetchMock = vi.fn(async (path: string, options: RequestInit) => {
        if (options.method !== 'POST') return ok(path === personalPath ? personal : { settings, policy });
        const { expected_revision, ...fields } = JSON.parse(options.body as string);
        return ok({ settings: { revision: expected_revision + 1, ...fields }, policy });
    });
    vi.stubGlobal('fetch', fetchMock);
    open(); await ready();
    fireEvent.click(screen.getByRole('switch', { name: 'Automatic memory' }));
    fireEvent.click(screen.getByRole('button', { name: 'Save memory and history settings' }));
    await screen.findByText('Saved.');
    const posts = fetchMock.mock.calls.filter(([, options]) => options.method === 'POST');
    expect(posts).toHaveLength(1);
    expect(posts[0][0]).toBe(settingsPath);
    expect(JSON.parse(posts[0][1].body as string)).toEqual({ expected_revision: 11,
        automatic_memory_enabled: false, model_history_search_enabled: true, explanation_version: 1 });
    expect(screen.getByRole('link', { name: 'Manage long-term memory' })).toHaveAttribute('href', '/ai-assistant/memories');
    expect(screen.getByLabelText('Personal instructions')).toHaveValue(personal.content);
});

it('keeps a stale draft and does not automatically overwrite a newer server revision', async () => {
    const fetchMock = vi.fn(async (path: string, options: RequestInit) => options.method === 'POST'
        ? failed(deskErrorCodeEnum.REVISION_CONFLICT) : ok(path === personalPath ? personal : { settings, policy }));
    vi.stubGlobal('fetch', fetchMock);
    open(); await ready();
    fireEvent.change(screen.getByLabelText('Personal instructions'), { target: { value: 'My local draft' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save personal instructions' }));
    await screen.findByText(/Settings changed elsewhere. Your draft is kept/);
    expect(screen.getByLabelText('Personal instructions')).toHaveValue('My local draft');
    expect(screen.queryByText('Saved.')).toBeNull();
    expect(fetchMock.mock.calls.filter(([, options]) => options.method === 'POST')).toHaveLength(1);
    expect(fetchMock).toHaveBeenCalledTimes(3);
});

it('keeps one settings section usable when the other fails to load', async () => {
    vi.stubGlobal('fetch', vi.fn(async (path: string) => path === personalPath
        ? failed(deskErrorCodeEnum.SYSTEM_ERROR) : ok({ settings, policy })));
    open();
    await screen.findByText(/The request did not complete. Your draft is kept/);
    await screen.findByRole('switch', { name: 'Automatic memory' });
    fireEvent.click(screen.getByRole('switch', { name: 'Search other conversations' }));
    expect(screen.getByRole('button', { name: 'Save memory and history settings' })).toBeEnabled();
});

it('sends only one write while the first save is still pending', async () => {
    let finish!: (value: ReturnType<typeof ok>) => void;
    const pending = new Promise<ReturnType<typeof ok>>(resolve => { finish = resolve; });
    const fetchMock = vi.fn(async (path: string, options: RequestInit) => options.method === 'POST'
        ? pending : ok(path === personalPath ? personal : { settings, policy }));
    vi.stubGlobal('fetch', fetchMock);
    open(); await ready();
    fireEvent.change(screen.getByLabelText('Personal instructions'), { target: { value: 'One write' } });
    const button = screen.getByRole('button', { name: 'Save personal instructions' });
    fireEvent.click(button); fireEvent.click(button);
    expect(fetchMock.mock.calls.filter(([, options]) => options.method === 'POST')).toHaveLength(1);
    await act(async () => { finish(ok({ revision: 8, content: 'One write' })); });
    await screen.findByText('Saved.');
});

it('uses the configured personalization capacity and still reads previously saved larger content', async () => {
    const configured = { ...policy, limits: { ...policy.limits, memory: { user_instructions_bytes: 16384 } } };
    const fetchMock = vi.fn(async (path: string, options: RequestInit) => {
        if (options.method !== 'POST') return ok(path === personalPath ? personal : { settings, policy: configured });
        const body = JSON.parse(options.body as string);
        return ok({ revision: body.expected_revision + 1, content: body.content });
    });
    vi.stubGlobal('fetch', fetchMock); open(); await ready();
    fireEvent.change(screen.getByLabelText('Personal instructions'), { target: { value: '中'.repeat(3000) } });
    expect(screen.getByText('9000 / 16384 bytes used')).toBeVisible();
    expect(screen.getByRole('button', { name: 'Save personal instructions' })).toBeEnabled();
    fireEvent.click(screen.getByRole('button', { name: 'Save personal instructions' }));
    await waitFor(() => expect(fetchMock.mock.calls.filter(([, options]) => options.method === 'POST')).toHaveLength(1));
});
