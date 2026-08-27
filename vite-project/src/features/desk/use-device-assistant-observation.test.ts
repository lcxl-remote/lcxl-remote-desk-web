import { act, renderHook } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import {
    SIGNALING_TYPE_CODE_AGENT_CAPABILITY_COMPLETED,
    SIGNALING_TYPE_CODE_INVOKE_AGENT_CAPABILITY,
} from './constants';
import type { SignalingSubscriber } from './use-desk-signaling';
import { useDeviceAssistantObservation } from './use-device-assistant-observation';

describe('useDeviceAssistantObservation', () => {
    it('sends only the typed read_context Computer Use observations', () => {
        let subscriber: SignalingSubscriber | null = null;
        const subscribe = vi.fn((handler: SignalingSubscriber) => {
            subscriber = handler;
            return () => { subscriber = null; };
        });
        const sendMessage = vi.fn(() => 'request-1');
        const { result } = renderHook(() => useDeviceAssistantObservation({
            deskId: 'desk-1',
            subscribe,
            sendMessage,
        }));

        act(() => { result.current.inspectSession(); });

        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_INVOKE_AGENT_CAPABILITY,
            {
                operation: {
                    risk_hint: null,
                    input: {
                        kind: 'read_context',
                        params: {
                            kind: {
                                kind: 'desktop_session_inspect',
                                params: { include_active_application: true },
                            },
                        },
                    },
                },
                reason: 'Device Assistant read-only observation preview',
            },
            'desk-1',
        );
        expect(result.current.entries.desktop_session_inspect.phase).toBe('pending');

        act(() => {
            subscriber?.({
                request_id: 'request-1',
                signaling_type: SIGNALING_TYPE_CODE_AGENT_CAPABILITY_COMPLETED,
                signaling_data: {
                    status: 'Ok',
                    data: { ReadContext: { DesktopSessionInspect: {} } },
                },
            });
        });
        expect(result.current.entries.desktop_session_inspect.phase).toBe('ready');
    });

    it('does not let an unrelated response settle the pending observation', () => {
        let subscriber: SignalingSubscriber | null = null;
        const subscribe = (handler: SignalingSubscriber) => {
            subscriber = handler;
            return () => { subscriber = null; };
        };
        const { result } = renderHook(() => useDeviceAssistantObservation({
            deskId: 'desk-1',
            subscribe,
            sendMessage: () => 'request-1',
        }));

        act(() => { result.current.inspectUi(); });
        act(() => {
            subscriber?.({
                request_id: 'other-request',
                signaling_type: SIGNALING_TYPE_CODE_AGENT_CAPABILITY_COMPLETED,
                signaling_data: { status: 'ok', data: {} },
            });
        });
        expect(result.current.entries.desktop_ui_inspect.phase).toBe('pending');
    });
});
