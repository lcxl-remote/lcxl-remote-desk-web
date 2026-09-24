import { act, renderHook, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { useAiAssistantChat, type RehearsalConversation } from './use-ai-assistant-chat';
import { SIGNALING_TYPE_CODE_ASK_AI_ASSISTANT } from './constants';

const intent: RehearsalConversation = {
    client_conversation_id: 'rehearsal_reserved',
    initial_message_id: 'rehearsal:reserved:input',
    prompt: '  frozen requirement  ',
    locale: 'zh-CN',
    status: 'pending',
};
const subscribe = () => () => {};
afterEach(() => { vi.unstubAllGlobals(); localStorage.clear(); });

describe('reserved rehearsal conversation', () => {
    it('pins the reserved identity and sends the frozen input once without overwriting normal history', async () => {
        localStorage.setItem('ai-assistant-conversation:device', 'ordinary-conversation');
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: null }) }));
        const sendMessage = vi.fn(() => 'request');
        const { result, unmount } = renderHook(() => useAiAssistantChat({
            deskId: 'device', rehearsal: intent, subscribe, sendMessage,
        }));
        await waitFor(() => expect(result.current.hydrating).toBe(false));
        expect(result.current.conversationId).toBe(intent.client_conversation_id);
        act(() => {
            expect(result.current.updateContext(['read'])).toBe(false);
            expect(result.current.detachAttachment('old')).toBe(false);
            expect(result.current.updateDirectory({ kind: 'select_directory', path: '/tmp', purpose: 'test', expected_revision: 0 }, 'timeout')).toBe(false);
            expect(result.current.selectConversation('ordinary-conversation')).toBe(false);
            result.current.reset();
            window.dispatchEvent(new StorageEvent('storage', { key: 'ai-assistant-conversation:device', newValue: 'other-tab' }));
            expect(result.current.start('changed requirement')).toBe(false);
        });
        expect(sendMessage).not.toHaveBeenCalled();
        act(() => { expect(result.current.start(intent.prompt, 'en-US', ['read'])).toBe(true); });
        expect(sendMessage).toHaveBeenCalledExactlyOnceWith(SIGNALING_TYPE_CODE_ASK_AI_ASSISTANT, {
            question: intent.prompt, client_message_id: intent.initial_message_id,
            conversation_id: intent.client_conversation_id, locale: 'zh-CN',
            selected_capability_ids: ['read'], selected_attachment_ids: [],
            start_goal: false, previous_completed_goal_id: null,
        }, 'device', expect.stringMatching(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/));
        act(() => { expect(result.current.start(intent.prompt)).toBe(false); result.current.reset(); });
        expect(sendMessage).toHaveBeenCalledTimes(1);
        expect(localStorage.getItem('ai-assistant-conversation:device')).toBe('ordinary-conversation');
        unmount();
    });

    it.each(['running', 'completed', 'cancelled', 'failed'] as const)('cannot restart a %s rehearsal', async status => {
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: null }) }));
        const sendMessage = vi.fn(() => 'request');
        const { result, unmount } = renderHook(() => useAiAssistantChat({
            deskId: 'device', rehearsal: { ...intent, status }, subscribe, sendMessage,
        }));
        await waitFor(() => expect(result.current.hydrating).toBe(false));
        act(() => { expect(result.current.start(intent.prompt)).toBe(false); });
        expect(sendMessage).not.toHaveBeenCalled();
        unmount();
    });
});
