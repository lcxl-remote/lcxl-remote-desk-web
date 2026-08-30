import { act, renderHook } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import {
    SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_CAPABILITIES_UPDATED,
    SIGNALING_TYPE_CODE_GET_DEVICE_ASSISTANT_CAPABILITIES,
} from './constants';
import type { SignalingSubscriber } from './use-desk-signaling';
import { useDeviceAssistantCapabilities } from './use-device-assistant-capabilities';

describe('useDeviceAssistantCapabilities', () => {
    it('requests and accepts only the correlated secret-free inventory response', () => {
        let subscriber: SignalingSubscriber | null = null;
        const subscribe = (handler: SignalingSubscriber) => {
            subscriber = handler;
            return () => { subscriber = null; };
        };
        const sendMessage = vi.fn(() => 'inventory-1');
        const { result } = renderHook(() => useDeviceAssistantCapabilities({
            deskId: 'desk-1',
            subscribe,
            sendMessage,
        }));

        act(() => { result.current.refresh(); });
        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_GET_DEVICE_ASSISTANT_CAPABILITIES,
            {},
            'desk-1',
        );

        act(() => {
            subscriber?.({
                request_id: 'other',
                signaling_type: SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_CAPABILITIES_UPDATED,
                signaling_data: { schema_version: 1, entries: [] },
            });
        });
        expect(result.current.snapshot).toBeNull();

        act(() => {
            subscriber?.({
                request_id: 'inventory-1',
                signaling_type: SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_CAPABILITIES_UPDATED,
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
    });
});
