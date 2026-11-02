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


describe('whiteboard layout cancellation', () => {
    it('discards an unfinished stroke without clearing remote content or active mode', () => {
        const video = document.createElement('video');
        Object.defineProperties(video, { videoWidth: { value: 1920 }, videoHeight: { value: 1080 } });
        video.getBoundingClientRect = () => ({ left: 0, top: 0, right: 320, bottom: 180, width: 320, height: 180 }) as DOMRect;
        const send = vi.fn();
        const { result } = renderHook(() => useDeskWhiteboard({ videoRef: { current: video }, whiteboardChannel: { current: { readyState: 'open', send } as unknown as RTCDataChannel }, isConnected: true, hasTauri: true }));
        act(() => result.current.toggleWhiteboard());
        act(() => result.current.handlePointerDown({ clientX: 30, clientY: 30 } as React.PointerEvent));
        act(() => result.current.handlePointerMove({ clientX: 80, clientY: 70 } as React.PointerEvent));
        expect(result.current.elements.length).toBe(1);
        act(() => result.current.cancelStroke());
        act(() => result.current.handlePointerUp());
        expect(result.current.elements).toEqual([]);
        expect(result.current.isActive).toBe(true);
        expect(send).not.toHaveBeenCalled();
    });
    it('reprojects an unsubmitted text anchor when the viewport origin changes', () => {
        const video = document.createElement('video');
        Object.defineProperties(video, { videoWidth: { value: 1920 }, videoHeight: { value: 1080 } });
        let left = 0;
        video.getBoundingClientRect = () => ({ left, top: 0, right: left + 640, bottom: 360, width: 640, height: 360 }) as DOMRect;
        const videoRef = { current: video }, whiteboardChannel = { current: { readyState: 'open', send: vi.fn() } as unknown as RTCDataChannel };
        const { result } = renderHook(() => useDeskWhiteboard({ videoRef, whiteboardChannel, isConnected: true, hasTauri: true }));
        act(() => { result.current.toggleWhiteboard(); result.current.setTool('text'); });
        act(() => result.current.handleCanvasClick({ clientX: 160, clientY: 90, stopPropagation() {} } as React.MouseEvent));
        const before = result.current.textInput!;
        left = 56;
        act(() => result.current.reflowTextInput());
        expect(result.current.textInput).toMatchObject({ x: before.x, y: before.y, clientX: before.clientX + 56 });
        expect(whiteboardChannel.current.send).not.toHaveBeenCalled();
    });
});
