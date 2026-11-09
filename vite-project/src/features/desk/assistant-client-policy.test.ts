import { assistantSourceId } from '@/test/assistant-source-id';
import { afterEach, expect, it, vi } from 'vitest';
import { assistantCitations, getAssistantProcessing, listAssistantMemories, listRetainedAssistantConversations,
    readAssistantRetainedHistory, readAssistantSourceHistory, searchAssistantHistory } from './assistant-data-api';

function nativeId(prefix: string, index: number) {
    const bytes = new Uint8Array(16); bytes[15] = index;
    return `${prefix}_${btoa(String.fromCharCode(...bytes)).replaceAll('+', '-').replaceAll('/', '_').replaceAll('=', '')}`;
}
const content = '引用🙂'.repeat(7000);
const source = (index = 0) => ({ source_id: assistantSourceId(index + 1), revision: 1, availability: 'available' as const,
    content, recorded_at_unix_ms: 100, statement_timezone: 'UTC', source_device: null, notice: null,
    locator: { conversation_id: 'original', root_conversation_id: 'original', message_id: 'answer',
        input_event_id: null, action_id: null, result_id: null, fragment_index: 0,
        byte_start: 0, byte_end: new TextEncoder().encode(content).length } });
const fragments = Array.from({ length: 25 }, (_, index) => ({ source: source(index), role: 'assistant', kind: 'assistant_message' }));
function respond(data: unknown) {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ success: true, code: 0, data }) }));
}
afterEach(() => vi.unstubAllGlobals());

it('accepts server memory pages above the former client count and still rejects duplicate identities', async () => {
    const memories = Array.from({ length: 51 }, (_, index) => ({ memory_id: nativeId('mem', index), revision: 1,
        scope: 'personal', content: 'Saved fact', context: null, status: 'active', source_ids: [assistantSourceId(index + 1)] }));
    const page = { memories, next_cursor: null, memory_state_generation: 1, control_generation: 1 };
    respond(page);
    expect((await listAssistantMemories()).memories).toHaveLength(51);
    respond({ ...page, memories: [memories[0], memories[0]] });
    await expect(listAssistantMemories()).rejects.toThrow();
});

it('accepts large complete source pages while checking their original session and anchor', async () => {
    const messages = Array.from({ length: 101 }, (_, index) => ({ id: index ? `message-${index}` : 'answer', role: 'assistant', text: content }));
    const page = { sessionId: 'original', messages, messagePage: { hasMore: false, limit: 200 } };
    respond(page);
    expect((await readAssistantSourceHistory(source(), { view: 'around' })).messages).toHaveLength(101);
    respond({ ...page, sessionId: 'other' });
    await expect(readAssistantSourceHistory(source(), { view: 'around' })).rejects.toThrow();
});

it('accepts server retained pages and long cursors without a second count or byte quota', async () => {
    const cursor = `rh1_${'x'.repeat(20000)}`;
    const page = { conversation_id: 'original', anchor_source_id: source().source_id, anchor_revision: 1,
        observed_through_generation: 1, fragments, previous_cursor: cursor, next_cursor: null, partial: false };
    respond(page);
    expect((await readAssistantRetainedHistory(source(), 'context')).fragments).toHaveLength(25);
    await expect(readAssistantRetainedHistory(source(), 'context', cursor)).rejects.toThrow();
});

it('accepts server search pages with larger fragments, reason lists and opaque cursors', async () => {
    const cursor = `oh1_${'x'.repeat(20000)}`;
    const page = { fragments, next_cursor: cursor, observed_through_generation: 1, partial: false,
        reasons: Array.from({ length: 9 }, (_, index) => `reason-${index}`) };
    const input = { queries: ['report'], sort: 'relevance' as const, limit: 10, cursor: null,
        device_ref: null, conversation_id: null, time_range: null, source_kinds: [] };
    respond(page);
    expect((await searchAssistantHistory(input)).fragments).toHaveLength(25);
    await expect(searchAssistantHistory({ ...input, cursor })).rejects.toThrow();
});

it('accepts a larger server conversation list and preserves root-source matching', async () => {
    const conversations = fragments.map((fragment, index) => ({ conversation_id: `root-${index}`,
        source: { ...fragment.source, locator: { ...fragment.source.locator, conversation_id: `root-${index}`, root_conversation_id: `root-${index}` } },
        execution_available: false, active: false }));
    respond({ conversations, next_cursor: `hc1_${'x'.repeat(2000)}`, partial: false });
    expect((await listRetainedAssistantConversations()).conversations).toHaveLength(25);
    respond({ conversations: [{ ...conversations[0], conversation_id: 'other' }], next_cursor: null, partial: false });
    await expect(listRetainedAssistantConversations()).rejects.toThrow();
});

it('keeps large citation collections bound to the exact answer and source revisions', () => {
    const citations = Array.from({ length: 100 }, (_, index) => ({ answer_message_id: 'answer', answer_revision: 1,
        source_id: assistantSourceId(index + 1), source_revision: 1 }));
    expect(new TextEncoder().encode(JSON.stringify(citations)).length).toBeGreaterThan(8192);
    expect(assistantCitations({ id: 'answer', citations })).toHaveLength(100);
    expect(assistantCitations({ id: 'other', citations })).toEqual([]);
    expect(assistantCitations({ id: 'answer', citations: [...citations, citations[0]] })).toEqual([]);
});

it('accepts server processing summaries above the former display counts', async () => {
    respond({ observed_at_unix_ms: 100, history_retention_days: 90, history_generation: 1,
        stored_history_fragments: 1, indexed_terms: 1, partial: false,
        coverage: Array.from({ length: 51 }, (_, index) => ({ reason: `reason-${index}` })),
        maintenance: { jobs: Array.from({ length: 33 }, (_, index) => ({ state: `state-${index}`, count: 1 })),
            spent_tokens: 0, reserved_tokens: 0, token_limit: 100 } });
    const result = await getAssistantProcessing();
    expect(result.coverage).toHaveLength(51); expect(result.maintenance.jobs).toHaveLength(33);
});
