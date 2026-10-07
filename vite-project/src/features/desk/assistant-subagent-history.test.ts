import { describe, expect, it } from 'vitest';
import type { PersistedSnapshot } from './use-ai-assistant-chat';
import { prependSubagentHistory, refreshSubagentHistory } from './assistant-subagent-history';

const page = (ids: string[], before: string | null, more: boolean) => ({
    messages: ids.map(id => ({ id, role: 'assistant', text: id })),
    messagePage: { hasMore: more, nextBeforeMessageId: before, limit: 100 },
} as PersistedSnapshot);

describe('child history windows', () => {
    it('preserves an exhausted older cursor while polling updates the recent window', () => {
        const recent = refreshSubagentHistory(null, page(['latest'], 'older', true));
        const expanded = prependSubagentHistory(recent, page(['first', 'latest'], null, false));
        const refreshed = refreshSubagentHistory(expanded, page(['latest', 'new'], 'older', true));
        expect(refreshed.messages.map(message => message.id)).toEqual(['first', 'latest', 'new']);
        expect(refreshed.hasMore).toBe(false);
        expect(refreshed.nextBefore).toBeNull();
    });

    it('keeps the coherent latest content when an older page repeats the same message', () => {
        const recent = refreshSubagentHistory(null, page(['latest'], 'older', true));
        recent.messages[0].text = 'current content';
        const expanded = prependSubagentHistory(recent, page(['first', 'latest'], null, false));
        expect(expanded.messages[1].text).toBe('current content');
    });
});
