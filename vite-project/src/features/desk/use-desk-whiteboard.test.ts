import { act, renderHook } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { useDeskWhiteboard } from './use-desk-whiteboard';

describe('useDeskWhiteboard connection handoff', () => {
    it('keeps active-mode intent but suspends interaction while the replacement PC is down', () => {
        const videoRef = { current: document.createElement('video') };
        const whiteboardChannel = {
            current: { readyState: 'open', send: vi.fn() } as unknown as RTCDataChannel,
        };
        const { result, rerender } = renderHook(
            ({ connected }) => useDeskWhiteboard({
                videoRef,
                whiteboardChannel,
                isConnected: connected,
                hasTauri: true,
            }),
            { initialProps: { connected: true } },
        );

        act(() => result.current.toggleWhiteboard());
        expect(result.current.isActive).toBe(true);
        expect(result.current.isInteractive).toBe(true);

        rerender({ connected: false });
        expect(result.current.isActive).toBe(true);
        expect(result.current.isInteractive).toBe(false);

        rerender({ connected: true });
        expect(result.current.isInteractive).toBe(true);
    });

    it('clears the host overlay when active mode is explicitly deactivated', () => {
        const send = vi.fn();
        const videoRef = { current: document.createElement('video') };
        const whiteboardChannel = {
            current: { readyState: 'open', send } as unknown as RTCDataChannel,
        };
        const { result } = renderHook(() => useDeskWhiteboard({
            videoRef,
            whiteboardChannel,
            isConnected: true,
            hasTauri: true,
        }));

        act(() => result.current.toggleWhiteboard());
        act(() => result.current.deactivateWhiteboard());

        expect(result.current.isActive).toBe(false);
        expect(send).toHaveBeenCalledWith(JSON.stringify({ type: 'clear' }));
    });
});
