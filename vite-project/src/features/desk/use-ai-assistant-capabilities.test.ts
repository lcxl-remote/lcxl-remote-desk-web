import { act, renderHook } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import {
    SIGNALING_TYPE_CODE_AI_ASSISTANT_CAPABILITIES_UPDATED,
    SIGNALING_TYPE_CODE_GET_AI_ASSISTANT_CAPABILITIES,
} from './constants';
import type { SignalingSubscriber } from './use-desk-signaling';
import { useAiAssistantCapabilities } from './use-ai-assistant-capabilities';

describe('useAiAssistantCapabilities', () => {
    it('requests and accepts only the correlated secret-free inventory response', () => {
        let subscriber: SignalingSubscriber | null = null;
        const subscribe = (handler: SignalingSubscriber) => {
            subscriber = handler;
            return () => { subscriber = null; };
        };
        const sendMessage = vi.fn(() => 'inventory-1');
        const { result } = renderHook(() => useAiAssistantCapabilities({
            deskId: 'desk-1',
            subscribe,
            sendMessage,
        }));

        expect(sendMessage).toHaveBeenCalledTimes(1);
        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_GET_AI_ASSISTANT_CAPABILITIES,
            {},
            'desk-1',
        );

        act(() => {
            subscriber?.({
                request_id: 'other',
                signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_CAPABILITIES_UPDATED,
                signaling_data: { schema_version: 1, entries: [] },
            });
        });
        expect(result.current.snapshot).toBeNull();

        act(() => {
            subscriber?.({
                request_id: 'inventory-1',
                signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_CAPABILITIES_UPDATED,
                signaling_data: {
                    schema_version: 1,
                    surface: 'oss_personal_owner',
                    generated_at_unix_ms: 1,
                    entries: [{
                        provider_id: 'office.document',
                        capability: { capability_id: 'office.document.inspect' },
                        context_selectable: true,
                        compiled: true,
                        enabled: true,
                        connected: true,
                        ready: false,
                        reason: 'office_bridge_not_paired',
                    }],
                },
            });
        });
        expect(result.current.snapshot?.entries[0].reason)
            .toBe('office_bridge_not_paired');
        expect(result.current.snapshot?.entries[0].context_selectable).toBe(true);
        expect(result.current.loading).toBe(false);
        act(() => { window.dispatchEvent(new Event('focus')); });
        expect(sendMessage).toHaveBeenCalledTimes(2);
        act(() => { window.dispatchEvent(new Event('focus')); });
        expect(sendMessage).toHaveBeenCalledTimes(2);
    });

    it('refreshes on reconnect and does not request capabilities while disabled', () => {
        const subscribe = vi.fn(() => () => {});
        const sendMessage = vi.fn(() => 'inventory');
        const { rerender, unmount } = renderHook(({ enabled }) => useAiAssistantCapabilities({
            deskId: 'desk-1', subscribe, sendMessage, enabled,
        }), { initialProps: { enabled: false } });
        act(() => { window.dispatchEvent(new Event('focus')); });
        expect(sendMessage).not.toHaveBeenCalled();
        rerender({ enabled: true });
        expect(sendMessage).toHaveBeenCalledTimes(1);
        rerender({ enabled: false });
        act(() => { window.dispatchEvent(new Event('focus')); });
        expect(sendMessage).toHaveBeenCalledTimes(1);
        rerender({ enabled: true });
        expect(sendMessage).toHaveBeenCalledTimes(2);
        unmount();
        act(() => { window.dispatchEvent(new Event('focus')); });
        expect(sendMessage).toHaveBeenCalledTimes(2);
    });
});
