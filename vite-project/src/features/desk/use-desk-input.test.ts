import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useDeskInput, buildKeyboardEventSequence, buildPhysicalKeyboardEvent, normalizeWheelDelta } from './use-desk-input';

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

describe('normalizeWheelDelta', () => {
    it('preserves pixel-mode deltas', () => {
        expect(normalizeWheelDelta(100, WheelEvent.DOM_DELTA_PIXEL, 1080)).toBe(100);
        expect(normalizeWheelDelta(-2.5, WheelEvent.DOM_DELTA_PIXEL, 1080)).toBe(-2.5);
    });

    it('normalizes line-mode deltas to pixels', () => {
        expect(normalizeWheelDelta(3, WheelEvent.DOM_DELTA_LINE, 1080)).toBe(120);
        expect(normalizeWheelDelta(-3, WheelEvent.DOM_DELTA_LINE, 1080)).toBe(-120);
    });

    it('normalizes page-mode deltas against the rendered axis', () => {
        expect(normalizeWheelDelta(1, WheelEvent.DOM_DELTA_PAGE, 1080)).toBe(1080);
        expect(normalizeWheelDelta(-1, WheelEvent.DOM_DELTA_PAGE, 1920)).toBe(-1920);
    });

    it('drops non-finite deltas before serialization', () => {
        expect(normalizeWheelDelta(Number.NaN, WheelEvent.DOM_DELTA_PIXEL, 1080)).toBe(0);
        expect(normalizeWheelDelta(Number.POSITIVE_INFINITY, WheelEvent.DOM_DELTA_PIXEL, 1080)).toBe(0);
    });
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

describe('buildKeyboardEventSequence — modifier state tracking', () => {
    it('reports the modifiers held at each step of a Ctrl+Alt+Del chord', () => {
        const seq = buildKeyboardEventSequence([
            { event: 'keydown', keyCode: 17 }, // Ctrl
            { event: 'keydown', keyCode: 18 }, // Alt
            { event: 'keydown', keyCode: 46 }, // Del
            { event: 'keyup', keyCode: 46 },
            { event: 'keyup', keyCode: 18 },
            { event: 'keyup', keyCode: 17 },
        ]);

        // Ctrl keydown: its own event already reflects ctrl held.
        expect(seq[0]).toMatchObject({ code: 'ControlLeft', key_code: 17, ctrl_key: true, alt_key: false });
        // Alt keydown: both modifiers now held.
        expect(seq[1]).toMatchObject({ key_code: 18, ctrl_key: true, alt_key: true });
        // Del down/up: carries both modifiers (previously hard-coded to false,
        // which dropped the chord on the macOS host).
        expect(seq[2]).toMatchObject({ code: 'Delete', key_code: 46, ctrl_key: true, alt_key: true });
        expect(seq[3]).toMatchObject({ key_code: 46, ctrl_key: true, alt_key: true });
        // Alt release clears only alt.
        expect(seq[4]).toMatchObject({ key_code: 18, ctrl_key: true, alt_key: false });
        // Ctrl release clears everything.
        expect(seq[5]).toMatchObject({ key_code: 17, ctrl_key: false, alt_key: false });
    });

    it('tracks the meta (Win/Cmd) key for both left and right key codes', () => {
        expect(buildKeyboardEventSequence([{ event: 'keydown', keyCode: 91 }])[0])
            .toMatchObject({ meta_key: true });
        expect(buildKeyboardEventSequence([{ event: 'keydown', keyCode: 92 }])[0])
            .toMatchObject({ meta_key: true });
    });

    it('leaves a plain key with no modifiers', () => {
        const [event] = buildKeyboardEventSequence([{ event: 'keydown', keyCode: 70 }]); // F
        expect(event).toMatchObject({
            code: 'KeyF',
            key_code: 70,
            ctrl_key: false,
            shift_key: false,
            alt_key: false,
            meta_key: false,
        });
    });

    it('adds the DOM code required by Linux Portal for PrintScreen', () => {
        const [event] = buildKeyboardEventSequence([{ event: 'keydown', keyCode: 44 }]);
        expect(event).toMatchObject({ code: 'PrintScreen', key_code: 44 });
    });
});

describe('buildPhysicalKeyboardEvent — desktop Ctrl compatibility for macOS', () => {
    const keyboardEvent = (overrides: Partial<KeyboardEvent>) => ({
        key: '',
        code: '',
        keyCode: 0,
        altKey: false,
        ctrlKey: false,
        shiftKey: false,
        metaKey: false,
        repeat: false,
        location: 0,
        isComposing: false,
        ...overrides,
    }) as KeyboardEvent;

    it('maps left Ctrl and its modifier flag to Command', () => {
        expect(buildPhysicalKeyboardEvent('keydown', keyboardEvent({
            key: 'Control',
            code: 'ControlLeft',
            keyCode: 17,
            ctrlKey: true,
        }), true)).toMatchObject({
            key_code: 91,
            ctrl_key: false,
            meta_key: true,
        });

        expect(buildPhysicalKeyboardEvent('keydown', keyboardEvent({
            key: 'c',
            code: 'KeyC',
            keyCode: 67,
            ctrlKey: true,
        }), true)).toMatchObject({
            key_code: 67,
            ctrl_key: false,
            meta_key: true,
        });
    });

    it('keeps right Ctrl as literal macOS Control for terminal chords', () => {
        expect(buildPhysicalKeyboardEvent('keydown', keyboardEvent({
            code: 'ControlRight',
            keyCode: 17,
            ctrlKey: true,
        }), true, { left: false, right: true })).toMatchObject({
            key_code: 17,
            ctrl_key: true,
            meta_key: false,
        });

        expect(buildPhysicalKeyboardEvent('keydown', keyboardEvent({
            key: 'c',
            code: 'KeyC',
            keyCode: 67,
            ctrlKey: true,
        }), true, { left: false, right: true })).toMatchObject({
            key_code: 67,
            ctrl_key: true,
            meta_key: false,
        });

        expect(buildPhysicalKeyboardEvent('keyup', keyboardEvent({
            code: 'ControlRight',
            keyCode: 17,
            ctrlKey: false,
        }), true, { left: false, right: false })).toMatchObject({
            key_code: 17,
            ctrl_key: false,
            meta_key: false,
        });
    });

    it('preserves both modifiers when left and right Ctrl are held together', () => {
        expect(buildPhysicalKeyboardEvent('keydown', keyboardEvent({
            key: 'k',
            code: 'KeyK',
            keyCode: 75,
            ctrlKey: true,
        }), true, { left: true, right: true })).toMatchObject({
            ctrl_key: true,
            meta_key: true,
        });
    });

    it('retains the literal Control mapping when compatibility is disabled', () => {
        expect(buildPhysicalKeyboardEvent('keydown', keyboardEvent({
            code: 'ControlLeft',
            keyCode: 17,
            ctrlKey: true,
        }), false)).toMatchObject({
            key_code: 17,
            ctrl_key: true,
            meta_key: false,
        });
    });
});

describe('useDeskInput — hidden page releases held keys', () => {
    function setup(remapCtrlToCommand = false) {
        const handlers: Record<string, Handler[]> = {};
        const element = makeVideo(handlers);
        const keyboardChannel = makeChannel();
        const mouseChannel = makeChannel();
        const videoRef = { current: element };
        renderHook(() =>
            useDeskInput({
                videoRef,
                mouseChannel: { current: mouseChannel as unknown as RTCDataChannel },
                keyboardChannel: { current: keyboardChannel as unknown as RTCDataChannel },
                isConnected: true,
                remapCtrlToCommand,
            }),
        );
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
        return { keyboardChannel, fire };
    }

    it('sends key-up for keys still held when the page becomes hidden', () => {
        const { keyboardChannel, fire } = setup();

        // Hold the Meta (Cmd) key — the classic macOS Cmd+Tab "stuck modifier"
        // scenario where the browser may never deliver its key-up.
        fire('keydown', { key: 'Meta', code: 'MetaLeft', keyCode: 91, metaKey: true });
        keyboardChannel.send.mockClear();

        Object.defineProperty(document, 'hidden', { value: true, configurable: true });
        act(() => {
            document.dispatchEvent(new Event('visibilitychange'));
        });

        const released = keyboardChannel.send.mock.calls
            .map(call => JSON.parse(call[0] as string))
            .filter(payload => payload.event === 'keyup' && payload.key_code === 91);
        expect(released).toHaveLength(1);
        expect(released[0].code).toBe('MetaLeft');

        Object.defineProperty(document, 'hidden', { value: false, configurable: true });
    });

    it('releases remapped Command on blur when Windows Ctrl is held', () => {
        const { keyboardChannel, fire } = setup(true);

        fire('keydown', {
            key: 'Control',
            code: 'ControlLeft',
            keyCode: 17,
            ctrlKey: true,
            metaKey: false,
        });
        keyboardChannel.send.mockClear();

        fire('blur', {});

        expect(lastSentPayload(keyboardChannel)).toMatchObject({
            event: 'keyup',
            code: 'ControlLeft',
            key_code: 91,
            ctrl_key: false,
            meta_key: false,
        });
    });

    it('releases literal Control on blur when right Ctrl is held', () => {
        const { keyboardChannel, fire } = setup(true);

        fire('keydown', {
            key: 'Control',
            code: 'ControlRight',
            keyCode: 17,
            ctrlKey: true,
            metaKey: false,
        });
        keyboardChannel.send.mockClear();

        fire('blur', {});

        expect(lastSentPayload(keyboardChannel)).toMatchObject({
            event: 'keyup',
            code: 'ControlRight',
            key_code: 17,
            ctrl_key: false,
            meta_key: false,
        });
    });

    it('sends right Ctrl+C as literal macOS Control+C', () => {
        const { keyboardChannel, fire } = setup(true);

        fire('keydown', {
            key: 'Control',
            code: 'ControlRight',
            keyCode: 17,
            ctrlKey: true,
            metaKey: false,
        });
        fire('keydown', {
            key: 'c',
            code: 'KeyC',
            keyCode: 67,
            ctrlKey: true,
            metaKey: false,
        });

        expect(lastSentPayload(keyboardChannel)).toMatchObject({
            event: 'keydown',
            key_code: 67,
            ctrl_key: true,
            meta_key: false,
        });
    });
});
