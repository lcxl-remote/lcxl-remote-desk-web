import { afterEach, describe, expect, it, vi } from 'vitest';
import { act, renderHook } from '@testing-library/react';

import {
    ownerSelectableWindows,
    useDeviceAssistantObservation,
    type ObservationEntry,
} from './use-device-assistant-observation';
import type { SignalingSubscriber } from './use-desk-signaling';
import { SIGNALING_TYPE_CODE_AGENT_CAPABILITY_COMPLETED } from './constants';

describe('delayed observation', () => {
    afterEach(() => vi.useRealTimers());
    function setup() {
        vi.useFakeTimers();
        const sendMessage = vi.fn(() => 'request-1');
        let subscriber: SignalingSubscriber = () => {};
        const subscribe = (callback: SignalingSubscriber) => { subscriber = callback; return () => {}; };
        const hook = renderHook(({ deskId, enabled }) => useDeviceAssistantObservation({ deskId, enabled, subscribe, sendMessage }), {
            initialProps: { deskId: 'device-1', enabled: true },
        });
        return { ...hook, sendMessage, reply: () => subscriber({
            signaling_type: SIGNALING_TYPE_CODE_AGENT_CAPABILITY_COMPLETED,
            request_id: 'request-1', signaling_data: { Ok: { target: 'old-device' } },
        } as Parameters<SignalingSubscriber>[0]) };
    }
    it('waits five seconds, suppresses duplicate clicks and sends one bounded request to the original device', () => {
        const { result, sendMessage } = setup();
        act(() => { result.current.scheduleUi(); result.current.scheduleUi(); result.current.inspectUi(); });
        expect(sendMessage).not.toHaveBeenCalled();
        act(() => vi.advanceTimersByTime(4_999));
        expect(sendMessage).not.toHaveBeenCalled();
        act(() => vi.advanceTimersByTime(1));
        expect(sendMessage).toHaveBeenCalledTimes(1);
        expect(sendMessage.mock.calls[0]).toEqual([expect.any(Number), expect.objectContaining({
            operation: { risk_hint: null, input: { kind: 'read_context', params: { kind: { kind: 'desktop_ui_inspect', params: { root: null, max_depth: 6, max_nodes: 300, max_bytes: 262144 } } } } },
        }), 'device-1']);
        act(() => result.current.scheduleUi());
        act(() => vi.advanceTimersByTime(5_000));
        expect(sendMessage).toHaveBeenCalledTimes(1);
    });
    it.each(['cancel', 'device', 'off', 'unmount'] as const)('cancels without collecting on %s', (reason) => {
        const { result, rerender, unmount, sendMessage } = setup();
        act(() => result.current.scheduleUi());
        if (reason === 'cancel') act(() => result.current.cancelDelayedUi());
        if (reason === 'device') rerender({ deskId: 'device-2', enabled: true });
        if (reason === 'off') rerender({ deskId: 'device-1', enabled: false });
        if (reason === 'unmount') unmount();
        act(() => vi.advanceTimersByTime(20_000));
        expect(sendMessage).not.toHaveBeenCalled();
    });
    it('discards old replies and timeouts after a device switch', () => {
        const { result, rerender, reply } = setup();
        act(() => result.current.inspectUi());
        rerender({ deskId: 'device-2', enabled: true });
        act(() => { reply(); vi.advanceTimersByTime(20_000); });
        expect(result.current.entries.desktop_ui_inspect.phase).toBe('idle');
    });
});

describe('ownerSelectableWindows', () => {
    it('projects only complete edge-issued window references', () => {
        const entry: ObservationEntry = {
            phase: 'ready',
            requestId: 'request-1',
            outcome: {
                status: 'ok',
                data: {
                    ReadContext: {
                        DesktopUiInspect: {
                            owner_selectable_windows: [{
                                object_ref: {
                                    token: 'opaque-window',
                                    snapshot_id: 'worker-1:7',
                                    object_kind: 'window',
                                    expires_at: '2030-01-01T00:00:00Z',
                                },
                                title: 'Calculator',
                            }, {
                                object_ref: {
                                    token: 'not-a-window',
                                    snapshot_id: 'worker-1:7',
                                    object_kind: 'ui_element',
                                    expires_at: '2030-01-01T00:00:00Z',
                                },
                            }],
                        },
                    },
                },
            },
        };

        expect(ownerSelectableWindows(entry)).toEqual([{
            objectRef: {
                token: 'opaque-window',
                snapshot_id: 'worker-1:7',
                object_kind: 'window',
                expires_at: '2030-01-01T00:00:00Z',
            },
            title: 'Calculator',
        }]);
    });
});
