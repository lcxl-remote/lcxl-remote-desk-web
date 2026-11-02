import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { Link, MemoryRouter, Route, Routes } from 'react-router-dom';
import { afterEach, expect, it, vi } from 'vitest';
import AiAssistantPage from './ai-assistant-page';
import { sourceTextParts } from './assistant-source-text';
import { assistantCitations, assistantSourceAnchor, readAssistantRetainedHistory, readAssistantSource } from './assistant-data-api';

vi.mock('@/services/clients', () => ({ listAssistantImages: vi.fn().mockResolvedValue({ data: [] }), getAssistantImage: vi.fn(), deleteAssistantImage: vi.fn() }));
vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(module => module.reactI18nextMock()));
const id = 'src_AAECAwQFBgcICQoLDA0ODw';
const source = { source_id:id,revision:1,availability:'available',content:'原始引用',recorded_at_unix_ms:100,statement_timezone:'UTC',source_device:null,
    locator:{conversation_id:'original-run',root_conversation_id:'original-run',message_id:'original-answer',input_event_id:null,action_id:null,result_id:null,
        content_sha256:'a'.repeat(64),fragment_index:0,byte_start:0,byte_end:12},notice:null };
const snapshot = (message='original-answer') => ({sessionId:'original-run',messages:[{id:'input',role:'user',text:'原来的问题'},
    {id:message,role:'assistant',text:'原始引用',toolCalls:[]}],messagePage:{hasMore:true,nextBeforeMessageId:'input',limit:100}});
const response = (data:unknown) => ({ok:true,json:async()=>({success:true,code:0,data})});
const show = (suffix='') => render(<MemoryRouter initialEntries={[`/ai-assistant/conversation?source=${id}${suffix.replace('?', '&')}`]}><Routes><Route path="/ai-assistant/conversation" element={<AiAssistantPage/>}/></Routes></MemoryRouter>);
afterEach(()=>vi.unstubAllGlobals());

it('accepts only bounded references for the exact native answer version', () => {
    const citation={answer_message_id:'answer',answer_revision:1,source_id:id,source_revision:2};
    expect(assistantCitations({id:'answer',citations:[citation]})).toEqual([{source_id:id,source_revision:2}]);
    expect(assistantCitations({id:'other',citations:[citation]})).toEqual([]);
    expect(assistantCitations({id:'answer',citations:[citation,citation]})).toEqual([]);
    expect(assistantCitations({id:'answer',citations:[{...citation,answer_revision:2}]})).toEqual([]);
});

it('refuses a different source revision before displaying text or fetching history', async () => {
    const fetcher=vi.fn().mockResolvedValueOnce(response(source));
    vi.stubGlobal('fetch',fetcher); show('?revision=2');
    expect(await screen.findByRole('alert')).toBeVisible();
    expect(fetcher).toHaveBeenCalledTimes(1);
    expect(screen.queryByText('原始引用')).toBeNull();
});

it('highlights only the exact UTF-8 native fragment, preserving Unicode boundaries and surrounding text', () => {
    const value = {...source, availability: 'available' as const, content:'引用🙂',
        locator:{...source.locator, byte_start:7,byte_end:17}};
    expect(sourceTextParts(value, {id:'original-answer',text:'前🙂引用🙂后'})).toEqual({before:'前🙂',match:'引用🙂',after:'后'});
    expect(sourceTextParts({...value,locator:{...value.locator,byte_start:4}}, {id:'original-answer',text:'前🙂引用🙂后'})).toBeNull();
    expect(sourceTextParts(value, {id:'original-answer',text:'不同原文'})).toBeNull();
    expect(sourceTextParts(value, {id:'another-answer',text:'前🙂引用🙂后'})).toBeNull();
    expect(sourceTextParts({...value,availability:'evidence_only',locator:null}, {id:'original-answer',text:'前🙂引用🙂后'})).toBeNull();
    expect(sourceTextParts({...value,locator:{...value.locator,message_id:null,result_id:'native-result-identity'}},
        {id:'native-result-identity',text:'前🙂引用🙂后'})).toBeNull();
});

it('loads an offline source at its native message using owner REST only, with no live controls', async () => {
    const fetcher=vi.fn().mockResolvedValueOnce(response(source)).mockResolvedValueOnce(response(snapshot()));
    vi.stubGlobal('fetch',fetcher); const socket=vi.fn(); vi.stubGlobal('WebSocket',socket);
    show();
    await waitFor(()=>expect(document.querySelector('[data-source-anchor="true"]')).not.toBeNull());
    expect(fetcher.mock.calls[1][0]).toBe('/api/my/ai-assistant-session?session=original-run&message_limit=100&message_around=original-answer');
    expect(fetcher.mock.calls[1][1]).toMatchObject({method:'GET',credentials:'include',cache:'no-store'});
    expect(socket).not.toHaveBeenCalled();
    expect(screen.queryByRole('textbox')).toBeNull();
    expect(screen.queryByRole('button',{name:/send|approve|execute/i})).toBeNull();
    expect(screen.getByText('原来的问题')).toBeVisible();
});

it.each(['native', 'retained'] as const)('scrolls to the bottom after loading the latest %s page', async kind => {
    const latest = { ...snapshot(), messages: [...snapshot().messages,
        { id: 'latest-answer', role: 'assistant', text: 'Newest answer' }] };
    const retained = { conversation_id: 'original-run', anchor_source_id: id, anchor_revision: 1,
        observed_through_generation: 7, fragments: [{ source: { ...source, content: 'Newest retained answer' },
            role: 'assistant', kind: 'assistant_message' }], previous_cursor: null, next_cursor: null, partial: false };
    const fetcher = vi.fn().mockResolvedValueOnce(response(source)).mockResolvedValueOnce(response(snapshot()))
        .mockResolvedValueOnce(response(source)).mockResolvedValueOnce(kind === 'native' ? response(latest) : { ok: false, status: 404 })
        .mockResolvedValueOnce(response(retained));
    vi.stubGlobal('fetch', fetcher); show();
    await screen.findByText('原来的问题');
    const scroller = screen.getByTestId('assistant-scroll-area');
    Object.defineProperty(scroller, 'scrollHeight', { configurable: true, value: 1800 });
    scroller.scrollTop = 120;
    fireEvent.click(screen.getByRole('button', { name: 'View latest conversation content' }));
    await screen.findByText(kind === 'native' ? 'Newest answer' : 'Newest retained answer');
    expect(fetcher.mock.calls[3][0]).toBe('/api/my/ai-assistant-session?session=original-run&message_limit=100');
    expect(scroller.scrollTop).toBe(1800);
    if (kind === 'retained') expect(fetcher.mock.calls[4][0]).toContain('view=latest');
});

it('preserves the reading position when earlier messages are prepended', async () => {
    const older = { ...snapshot(), messages: [{ id: 'older', role: 'user', text: 'Earlier question' }, ...snapshot().messages] };
    const fetcher = vi.fn().mockResolvedValueOnce(response(source)).mockResolvedValueOnce(response(snapshot()))
        .mockResolvedValueOnce(response(source)).mockResolvedValueOnce(response(older));
    vi.stubGlobal('fetch', fetcher);
    show(); await screen.findByText('原来的问题');
    const scroller = screen.getByTestId('assistant-scroll-area');
    Object.defineProperty(scroller, 'scrollHeight', { configurable: true, get: () => screen.queryByText('Earlier question') ? 1500 : 900 });
    scroller.scrollTop = 120;
    fireEvent.click(screen.getByRole('button', { name: 'Load earlier messages' }));
    await screen.findByText('Earlier question');
    expect(fetcher.mock.calls[3][0]).toBe('/api/my/ai-assistant-session?session=original-run&message_limit=100&message_before=input');
    expect([...scroller.querySelectorAll('[data-assistant-message-id]')].map(node => node.getAttribute('data-assistant-message-id')))
        .toEqual(['older', 'input', 'original-answer']);
    expect(scroller.scrollTop).toBe(720);
});

it('locates the original quotation again after viewing the latest content', async () => {
    const reveal = vi.spyOn(HTMLElement.prototype, 'scrollIntoView');
    try {
        const latest = { ...snapshot(), messages: [{ id: 'new-answer', role: 'assistant', text: 'Newest answer' }] };
        vi.stubGlobal('fetch', vi.fn().mockResolvedValueOnce(response(source)).mockResolvedValueOnce(response(snapshot()))
            .mockResolvedValueOnce(response(source)).mockResolvedValueOnce(response(latest))
            .mockResolvedValueOnce(response(source)).mockResolvedValueOnce(response(snapshot())));
        show(); await screen.findByText('原来的问题');
        await waitFor(() => expect(reveal).toHaveBeenCalledWith({ block: 'center' }));
        reveal.mockClear();
        fireEvent.click(screen.getByRole('button', { name: 'View latest conversation content' }));
        await screen.findByText('Newest answer');
        expect(reveal).not.toHaveBeenCalled();
        fireEvent.click(screen.getByRole('button', { name: 'Locate reference' }));
        await screen.findByText('原来的问题');
        await waitFor(() => {
            expect(reveal).toHaveBeenCalledWith({ block: 'center' });
            expect(document.activeElement?.getAttribute('data-assistant-message-id')).toBe('original-answer');
        });
    } finally { reveal.mockRestore(); }
});

it('refreshes the source for older pages and never opens a snapshot for independent or unavailable evidence', async () => {
    const fetcher=vi.fn().mockResolvedValueOnce(response(source)).mockResolvedValueOnce(response(snapshot()))
        .mockResolvedValueOnce(response({...source,availability:'evidence_only',locator:null}));
    vi.stubGlobal('fetch',fetcher); show();
    const earlier=await screen.findByRole('button',{name:'Load earlier messages'});
    fireEvent.click(earlier);
    await waitFor(()=>expect(fetcher).toHaveBeenCalledTimes(3));
    await waitFor(()=>expect(document.querySelector('[data-source-anchor="true"]')).toBeNull());
    expect(screen.getByText(/Independently retained evidence/)).toBeVisible();
    expect(screen.queryByText('原来的问题')).toBeNull();
});

it('refuses a snapshot that belongs to a different session or misses its actual anchor', async () => {
    vi.stubGlobal('fetch',vi.fn().mockResolvedValueOnce(response(source)).mockResolvedValueOnce(response(snapshot('wrong-anchor'))));
    show();
    expect(await screen.findByRole('alert')).toBeVisible();
    expect(document.querySelector('[data-source-anchor="true"]')).toBeNull();
    expect(screen.queryByText('原来的问题')).toBeNull();
    expect(screen.getByText('原始引用')).toBeVisible();
    expect(screen.getByText(/Only the source information that remains available/)).toBeVisible();
});

it('shows the legal original chat while keeping forgotten memory status explicit', async () => {
    vi.stubGlobal('fetch',vi.fn().mockResolvedValueOnce(response({...source,notice:'memory_forgotten_original_kept'})).mockResolvedValueOnce(response(snapshot())));
    show();
    expect(await screen.findByText('This memory version was forgotten. Its original chat remains available.')).toBeVisible();
    await waitFor(()=>expect(document.querySelector('[data-source-anchor="true"]')).not.toBeNull());
});

it('opens retained fragments after execution-state reclamation and rechecks the source before paging', async () => {
    const retained = {conversation_id:'original-run',anchor_source_id:id,anchor_revision:1,observed_through_generation:7,
        fragments:[{source,role:'assistant',kind:'assistant_message'}],previous_cursor:'rh1_older',next_cursor:null,partial:false};
    const fetcher=vi.fn().mockResolvedValueOnce(response(source))
        .mockResolvedValueOnce({ok:false,status:404})
        .mockResolvedValueOnce(response(retained))
        .mockResolvedValueOnce(response({...source,availability:'unavailable',revision:0,content:null,locator:null}));
    vi.stubGlobal('fetch',fetcher); show('?revision=1');
    await waitFor(()=>expect(fetcher).toHaveBeenCalledTimes(3));
    expect(await screen.findByText(/Retained history fragments/)).toBeVisible();
    expect(fetcher.mock.calls[2][0]).toBe(`/api/my/ai-assistant-history/window?source_id=${id}&revision=1&view=context`);
    expect(document.querySelector('[data-source-anchor="true"]')).not.toBeNull();
    expect(screen.queryByRole('textbox')).toBeNull();
    fireEvent.click(screen.getByRole('button',{name:'Load earlier messages'}));
    await waitFor(()=>expect(fetcher).toHaveBeenCalledTimes(4));
    await waitFor(()=>expect(screen.queryByText(/Retained history fragments/)).toBeNull());
    expect(document.querySelector('[data-source-anchor="true"]')).toBeNull();
});

it('never presents a retained page from another conversation as source history', async () => {
    vi.stubGlobal('fetch',vi.fn().mockResolvedValueOnce(response(source)).mockResolvedValueOnce({ok:false,status:404})
        .mockResolvedValueOnce(response({conversation_id:'other',anchor_source_id:id,anchor_revision:1,observed_through_generation:1,
            fragments:[{source,role:'user',kind:'input'}],previous_cursor:null,next_cursor:null,partial:false})));
    show(); expect(await screen.findByRole('alert')).toBeVisible();
    expect(document.querySelector('[data-source-anchor="true"]')).toBeNull();
});


it('reads and locates a permitted source larger than the former 32 KiB limit', async () => {
    const content = 'x'.repeat(64 * 1024);
    const large = { ...source, content, locator: { ...source.locator, byte_end: content.length } };
    vi.stubGlobal('fetch', vi.fn().mockResolvedValueOnce(response(large)));
    const received = await readAssistantSource(id, 1);
    expect(received.content).toBe(content);
    expect(assistantSourceAnchor(received)).toEqual({ session: 'original-run', message: 'original-answer' });
    expect(sourceTextParts(received, { id: 'original-answer', text: content })).toEqual({ before: '', match: content, after: '' });
    expect(sourceTextParts(received, { id: 'original-answer', text: content.slice(1) })).toBeNull();
});

it('accepts a complete retained window over 32 KiB and preserves its page and source checks', async () => {
    const content = 'x'.repeat(64 * 1024);
    const large = { ...source, availability: 'available' as const, content,
        locator: { ...source.locator, byte_end: content.length } };
    const retained = { conversation_id: 'original-run', anchor_source_id: id, anchor_revision: 1,
        observed_through_generation: 7, fragments: [{ source: large, role: 'assistant', kind: 'assistant_message' }],
        previous_cursor: 'rh1_older', next_cursor: null, partial: false };
    const fetcher = vi.fn().mockResolvedValueOnce(response(retained))
        .mockResolvedValueOnce(response({ ...retained, fragments: Array(21).fill(retained.fragments[0]) }))
        .mockResolvedValueOnce(response({ ...retained, conversation_id: 'other' }))
        .mockResolvedValueOnce(response(retained));
    vi.stubGlobal('fetch', fetcher);
    expect(await readAssistantRetainedHistory(large, 'context')).toEqual(retained);
    await expect(readAssistantRetainedHistory(large, 'context')).rejects.toThrow();
    await expect(readAssistantRetainedHistory(large, 'context')).rejects.toThrow();
    await expect(readAssistantRetainedHistory(large, 'context', 'rh1_older')).rejects.toThrow();
});

it('keeps assistant Markdown and focuses its body without selecting its attached tools', async () => {
    const page = snapshot();
    page.messages[1] = { ...page.messages[1], text: '**原始引用**', toolCalls: [{ id: 'lookup', name: 'search_history', argumentsJson: '{}' }] } as never;
    vi.stubGlobal('fetch', vi.fn().mockResolvedValueOnce(response(source)).mockResolvedValueOnce(response(page)));
    show();
    const anchor = await waitFor(() => {
        const elements = document.querySelectorAll('[data-source-anchor="true"]');
        expect(elements).toHaveLength(1); return elements[0];
    });
    expect(anchor.getAttribute('data-assistant-message-id')).toBe('original-answer');
    expect(anchor.querySelector('strong')?.textContent).toBe('原始引用');
    expect(screen.getByTestId('assistant-shared-transcript')).toBeVisible();
});

it('expands the actual tool group for a result with no separate message bubble', async () => {
    const page = { ...snapshot(), messages: [
        { id: 'first', role: 'assistant', text: '开始查找' },
        { id: 'call', role: 'assistant', text: '', toolCalls: [{ id: 'lookup', name: 'search_history', argumentsJson: '{}' }] },
        { id: 'original-answer', role: 'tool', text: '检索得到的原始结果', toolCallId: 'lookup' },
        { id: 'last', role: 'assistant', text: '结果汇总', reasoning: '检查结果' },
    ] };
    vi.stubGlobal('fetch', vi.fn().mockResolvedValueOnce(response(source)).mockResolvedValueOnce(response(page)));
    show();
    const anchor = await waitFor(() => {
        const element = document.querySelector('[data-source-anchor="true"]'); expect(element).not.toBeNull(); return element!;
    });
    expect(anchor.getAttribute('data-assistant-message-id')).toBe('tool-call-lookup');
    expect(anchor.closest('[data-state="closed"]')).toBeNull();
    expect(screen.getByText('检索得到的原始结果')).toBeVisible();
});

it('prepends older messages, deduplicates overlap and retains the original anchor', async () => {
    let resolveSource!: (value: unknown) => void;
    const refreshed = new Promise(resolve => { resolveSource = resolve; });
    const older = { ...snapshot(), messages: [{ id: 'earlier', role: 'user', text: '更早的问题' },
        { id: 'input', role: 'user', text: '重复页里的旧文本' }], messagePage: { hasMore: false, limit: 100 } };
    const fetcher = vi.fn().mockResolvedValueOnce(response(source)).mockResolvedValueOnce(response(snapshot()))
        .mockReturnValueOnce(refreshed).mockResolvedValueOnce(response(older));
    vi.stubGlobal('fetch', fetcher); show();
    fireEvent.click(await screen.findByRole('button', { name: 'Load earlier messages' }));
    expect(screen.queryByText('原来的问题')).toBeNull();
    resolveSource(response(source));
    expect(await screen.findByText('更早的问题')).toBeVisible();
    expect(screen.getAllByText('原来的问题')).toHaveLength(1);
    expect(screen.queryByText('重复页里的旧文本')).toBeNull();
    expect(document.querySelector('[data-source-anchor="true"]')?.getAttribute('data-assistant-message-id')).toBe('original-answer');
    expect(screen.queryByRole('button', { name: 'Load earlier messages' })).toBeNull();
});

it('ignores a late history response after selecting a different source', async () => {
    let finishOld!: (value: unknown) => void;
    const oldPage = new Promise(resolve => { finishOld = resolve; });
    const secondId = 'src_AQECAwQFBgcICQoLDA0ODw';
    const second = { ...source, source_id: secondId, locator: { ...source.locator,
        conversation_id: 'second-run', root_conversation_id: 'second-run', message_id: 'second-answer' } };
    const secondPage = { ...snapshot(), sessionId: 'second-run', messages: [{ id: 'second-answer', role: 'assistant', text: '另一段会话' }] };
    const fetcher = vi.fn().mockResolvedValueOnce(response(source)).mockReturnValueOnce(oldPage)
        .mockResolvedValueOnce(response(second)).mockResolvedValueOnce(response(secondPage));
    vi.stubGlobal('fetch', fetcher);
    render(<MemoryRouter initialEntries={[`/ai-assistant/conversation?source=${id}`]}>
        <Link to={`/ai-assistant/conversation?source=${secondId}`}>Another source</Link>
        <AiAssistantPage />
    </MemoryRouter>);
    await waitFor(() => expect(fetcher).toHaveBeenCalledTimes(2));
    fireEvent.click(screen.getByRole('link', { name: 'Another source' }));
    expect(await screen.findByText('另一段会话')).toBeVisible();
    await act(async () => { finishOld(response(snapshot())); });
    expect(screen.queryByText('原来的问题')).toBeNull();
    expect(document.querySelector('[data-source-anchor="true"]')?.getAttribute('data-assistant-message-id')).toBe('second-answer');
});
