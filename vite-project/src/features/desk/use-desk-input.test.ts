import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useDeskInput } from './use-desk-input';

// `useDeskInput` wires DOM listeners onto the <video> element and a few
// browser globals (ResizeObserver). jsdom does not implement
// ResizeObserver and leaves videoWidth/videoHeight at 0, so we stub both
// and drive the captured DOM handlers directly with plain event objects
// carrying only the fields the hook reads.

type Handler = (event: unknown) => void;

let resizeCallback: ((entries: { contentRect: { width: number; height: number } }[]) => void) | null = null;

class MockResizeObserver {
    constructor(cb: (entries: { contentRect: { width: number; height: number } }[]) => void) {
        resizeCallback = cb;
    }
    observe() {}
    unobserve() {}
    disconnect() {}
}

function makeChannel() {
    return {
        readyState: 'open' as RTCDataChannelState,
        send: vi.fn(),
    };
}

/** Build a <video> element with stubbed intrinsic size and a listener
 *  registry so the test can invoke the hook's handlers by event type. */
function makeVideo(handlers: Record<string, Handler[]>) {
    const element = document.createElement('video');
    Object.defineProperty(element, 'videoWidth', { value: 1920, configurable: true });
    Object.defineProperty(element, 'videoHeight', { value: 1080, configurable: true });
    element.focus = vi.fn();
    const realAdd = element.addEventListener.bind(element);
    vi.spyOn(element, 'addEventListener').mockImplementation((type: string, cb: EventListenerOrEventListenerObject, opts?: unknown) => {
        (handlers[type] ||= []).push(cb as Handler);
        realAdd(type, cb as EventListener, opts as AddEventListenerOptions);
    });
    return element;
}

function lastSentPayload(channel: ReturnType<typeof makeChannel>) {
    const calls = channel.send.mock.calls;
    return JSON.parse(calls[calls.length - 1][0] as string);
}

beforeEach(() => {
    resizeCallback = null;
    (globalThis as unknown as { ResizeObserver: typeof MockResizeObserver }).ResizeObserver = MockResizeObserver;
});

afterEach(() => {
    vi.restoreAllMocks();
});

describe('useDeskInput — blur release uses last known cursor position', () => {
    function setup() {
        const handlers: Record<string, Handler[]> = {};
        const element = makeVideo(handlers);
        const mouseChannel = makeChannel();
        const mouseMoveChannel = makeChannel();
        const keyboardChannel = makeChannel();
        const videoRef = { current: element };
        renderHook(() =>
            useDeskInput({
                videoRef,
                mouseChannel: { current: mouseChannel as unknown as RTCDataChannel },
                keyboardChannel: { current: keyboardChannel as unknown as RTCDataChannel },
                mouseMoveChannel: { current: mouseMoveChannel as unknown as RTCDataChannel },
                isConnected: true,
            }),
        );
        // Establish a non-zero rendered surface (1:1 with the stubbed
        // 1920x1080 video, so ratios map cleanly and there is no
        // letterboxing offset).
        act(() => {
            resizeCallback?.([{ contentRect: { width: 1920, height: 1080 } }]);
        });
        const fire = (type: string, event: Record<string, unknown>) => {
            act(() => {
                for (const cb of handlers[type] || []) {
                    cb({ preventDefault: () => {}, stopPropagation: () => {}, ...event });
                }
            });
        };
        return { handlers, mouseChannel, fire };
    }

    it('sends the synthetic mouseup at the last cursor position, not (0,0)', () => {
        const { mouseChannel, fire } = setup();

        // Press the left button at the surface center → ratio (0.5, 0.5).
        fire('mousedown', { offsetX: 960, offsetY: 540, button: 0, buttons: 1, altKey: false });
        expect(lastSentPayload(mouseChannel)).toMatchObject({ event: 'mousedown', x: 0.5, y: 0.5 });

        // Losing focus while the button is held must release at the same
        // point — previously this released at (0, 0), warping the remote
        // cursor to the top-left corner.
        fire('blur', {});
        const release = lastSentPayload(mouseChannel);
        expect(release).toMatchObject({ event: 'mouseup', button: 0, buttons: 0 });
        expect(release.x).toBeCloseTo(0.5);
        expect(release.y).toBeCloseTo(0.5);
    });

    it('tracks the most recent move so the blur release follows the cursor', () => {
        const { mouseChannel, fire } = setup();

        fire('mousedown', { offsetX: 0, offsetY: 0, button: 0, buttons: 1, altKey: false });
        // Move to three-quarters across before focus is lost.
        fire('mousemove', { offsetX: 1440, offsetY: 810, button: 0, buttons: 1, altKey: false });

        fire('blur', {});
        const release = lastSentPayload(mouseChannel);
        expect(release.event).toBe('mouseup');
        expect(release.x).toBeCloseTo(0.75);
        expect(release.y).toBeCloseTo(0.75);
    });

    it('does not send a release when no button was pressed', () => {
        const { mouseChannel, fire } = setup();

        fire('mousemove', { offsetX: 960, offsetY: 540, button: 0, buttons: 0, altKey: false });
        mouseChannel.send.mockClear();

        fire('blur', {});
        // No held button → nothing to release on the reliable channel.
        expect(mouseChannel.send).not.toHaveBeenCalled();
    });
});
