import { describe, it, expect, vi, beforeEach } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { computeCursorScale, useCursorSync } from './use-cursor-sync';

// Mock toast and translations: this test file focuses on the
// computeCursorScale pure function and a narrow embed-toast
// observation. We do not exercise the canvas / Image pipeline (per
// the codex review the jsdom-canvas surface is too brittle to lean
// on).
const toastMock = vi.fn();
vi.mock('@/hooks/use-toast', () => ({
    useToast: () => ({ toast: toastMock, dismiss: vi.fn(), toasts: [] }),
}));
vi.mock('react-i18next', () => ({
    useTranslation: () => ({
        t: (_key: string, fallback?: string) => fallback ?? _key,
    }),
}));

beforeEach(() => {
    toastMock.mockClear();
});

describe('computeCursorScale', () => {
    it('returns 0 when video native size is zero', () => {
        const s = computeCursorScale(
            { width: 1920, height: 1080 },
            { width: 0, height: 1080 },
            { width: 1920, height: 1080 },
        );
        expect(s).toBe(0);
    });

    it('returns 0 when screen size is zero', () => {
        const s = computeCursorScale(
            { width: 1920, height: 1080 },
            { width: 1920, height: 1080 },
            { width: 0, height: 0 },
        );
        expect(s).toBe(0);
    });

    it('returns 1.0 when video DOM = video native = screen (1:1)', () => {
        const s = computeCursorScale(
            { width: 1920, height: 1080 },
            { width: 1920, height: 1080 },
            { width: 1920, height: 1080 },
        );
        expect(s).toBe(1);
    });

    it('halves the cursor when DOM is half of video native (uniform scale)', () => {
        const s = computeCursorScale(
            { width: 960, height: 540 },
            { width: 1920, height: 1080 },
            { width: 1920, height: 1080 },
        );
        expect(s).toBeCloseTo(0.5);
    });

    it('picks the smaller axis ratio under letterbox (DOM wider than video aspect)', () => {
        // DOM is 1920x500 (wider relative to native 1920x1080). The
        // height ratio (500/1080 ≈ 0.463) is the bottleneck — the
        // width ratio is 1.0.
        const s = computeCursorScale(
            { width: 1920, height: 500 },
            { width: 1920, height: 1080 },
            { width: 1920, height: 1080 },
        );
        expect(s).toBeCloseTo(500 / 1080);
    });

    it('picks the smaller axis ratio under pillarbox (DOM taller than video aspect)', () => {
        // DOM is 800x1080 (taller). Width ratio is the bottleneck.
        const s = computeCursorScale(
            { width: 800, height: 1080 },
            { width: 1920, height: 1080 },
            { width: 1920, height: 1080 },
        );
        expect(s).toBeCloseTo(800 / 1920);
    });

    it('applies the encoder downsample factor when video native < screen', () => {
        // Hypothetical: backend captures 1920x1080 but encoder
        // outputs 1280x720. DOM is 1:1 with the encoded stream.
        // encoderRatio = 1280/1920 = 0.667; videoScale = 1.0
        const s = computeCursorScale(
            { width: 1280, height: 720 },
            { width: 1280, height: 720 },
            { width: 1920, height: 1080 },
        );
        expect(s).toBeCloseTo(1280 / 1920);
    });

    it('composes encoder and DOM ratios multiplicatively', () => {
        // DOM 960x540, encoded video 1280x720, captured screen
        // 1920x1080. domScale = 960/1280 = 0.75; encoderRatio =
        // 1280/1920 ≈ 0.667; result ≈ 0.5.
        const s = computeCursorScale(
            { width: 960, height: 540 },
            { width: 1280, height: 720 },
            { width: 1920, height: 1080 },
        );
        expect(s).toBeCloseTo(0.5);
    });
});

describe('useCursorSync — embed transition toast', () => {
    function makeMockChannel() {
        const listeners: Record<string, ((e: MessageEvent) => void)[]> = {};
        return {
            addEventListener: vi.fn((evt: string, cb: (e: MessageEvent) => void) => {
                (listeners[evt] ||= []).push(cb);
            }),
            removeEventListener: vi.fn((evt: string, cb: (e: MessageEvent) => void) => {
                listeners[evt] = (listeners[evt] || []).filter((x) => x !== cb);
            }),
            // Test helper — fire a synthetic message at every listener.
            __dispatch(payload: object) {
                const event = { data: JSON.stringify(payload) } as MessageEvent;
                for (const cb of listeners['message'] || []) {
                    cb(event);
                }
            },
        };
    }

    function makeRefs() {
        const channel = makeMockChannel();
        const cursorSyncChannel = { current: channel as unknown as RTCDataChannel };
        // The hook reads videoRef inside applyCursor only when
        // data.visible=true and embedded=false. The transitions we
        // assert on (embedded false→true) all send visible=false, so
        // a stub video is enough.
        const videoRef = { current: document.createElement('video') as HTMLVideoElement };
        return { cursorSyncChannel, videoRef, channel };
    }

    it('keeps the previous cursorStyle when embedded=true (does not hide the sprite)', () => {
        // Embedded mode means the OS has baked the cursor into the
        // video frame. We deliberately keep the local CSS cursor
        // visible because it tracks the user's actual mouse with no
        // video latency — losing that responsiveness would feel
        // worse than the double-cursor artefact.
        const { cursorSyncChannel, videoRef, channel } = makeRefs();
        const { result } = renderHook(() =>
            useCursorSync(cursorSyncChannel, videoRef, true),
        );

        // Before any data arrives the hook starts at 'default'.
        expect(result.current.cursorStyle).toBe('default');

        // Embedded payload (visible=false, embedded=true). The
        // hook must NOT switch to 'none' — it should leave the
        // cursorStyle untouched so the previously rendered sprite
        // (or the page default) keeps tracking the mouse.
        act(() => {
            channel.__dispatch({
                base64_png: '',
                hotspot_x: 0,
                hotspot_y: 0,
                visible: false,
                shape_id: 0,
                screen_width: 1920,
                screen_height: 1080,
                embedded: true,
            });
        });
        expect(result.current.cursorStyle).toBe('default');
    });

    it('hides the cursor when visible=false and embedded=false (genuine hidden)', () => {
        // Legitimate hidden-cursor states (IME entry, cursor confined,
        // text-tool with no caret) must still produce 'none' so the
        // page reflects the OS behaviour.
        const { cursorSyncChannel, videoRef, channel } = makeRefs();
        const { result } = renderHook(() =>
            useCursorSync(cursorSyncChannel, videoRef, true),
        );
        act(() => {
            channel.__dispatch({
                base64_png: '',
                hotspot_x: 0,
                hotspot_y: 0,
                visible: false,
                shape_id: 0,
                screen_width: 1920,
                screen_height: 1080,
                embedded: false,
            });
        });
        expect(result.current.cursorStyle).toBe('none');
    });

    it('fires the remote-cursor toast when embedded transitions false → true', () => {
        const { cursorSyncChannel, videoRef, channel } = makeRefs();
        renderHook(() => useCursorSync(cursorSyncChannel, videoRef, true));

        // Frame 1: hardware cursor (embedded=false) — no toast.
        act(() => {
            channel.__dispatch({
                base64_png: '',
                hotspot_x: 0,
                hotspot_y: 0,
                visible: true,
                shape_id: 1,
                screen_width: 1920,
                screen_height: 1080,
                embedded: false,
            });
        });
        expect(toastMock).not.toHaveBeenCalled();

        // Frame 2: software cursor (embedded=true) — toast must
        // fire exactly once on the rising edge.
        act(() => {
            channel.__dispatch({
                base64_png: '',
                hotspot_x: 0,
                hotspot_y: 0,
                visible: false,
                shape_id: 0,
                screen_width: 1920,
                screen_height: 1080,
                embedded: true,
            });
        });
        expect(toastMock).toHaveBeenCalledTimes(1);
    });

    it('does not refire the toast while embedded stays true', () => {
        const { cursorSyncChannel, videoRef, channel } = makeRefs();
        renderHook(() => useCursorSync(cursorSyncChannel, videoRef, true));

        for (let i = 0; i < 3; i += 1) {
            act(() => {
                channel.__dispatch({
                    base64_png: '',
                    hotspot_x: 0,
                    hotspot_y: 0,
                    visible: false,
                    shape_id: 0,
                    screen_width: 1920,
                    screen_height: 1080,
                    embedded: true,
                });
            });
        }
        expect(toastMock).toHaveBeenCalledTimes(1);
    });

    it('does not fire the toast when visible=false but embedded=false', () => {
        // Legitimate hidden-cursor states (IME entry, cursor confined)
        // must remain silent — only the embedded transition is the
        // user-visible behavioural change we want to surface.
        const { cursorSyncChannel, videoRef, channel } = makeRefs();
        renderHook(() => useCursorSync(cursorSyncChannel, videoRef, true));

        act(() => {
            channel.__dispatch({
                base64_png: '',
                hotspot_x: 0,
                hotspot_y: 0,
                visible: false,
                shape_id: 0,
                screen_width: 1920,
                screen_height: 1080,
                embedded: false,
            });
        });
        expect(toastMock).not.toHaveBeenCalled();
    });

    it('does not fire the toast when hasControl is false', () => {
        // No control = no local cursor sprite rendered, so the
        // "remote cursor took over" message is not relevant.
        const { cursorSyncChannel, videoRef, channel } = makeRefs();
        renderHook(() => useCursorSync(cursorSyncChannel, videoRef, false));

        act(() => {
            channel.__dispatch({
                base64_png: '',
                hotspot_x: 0,
                hotspot_y: 0,
                visible: false,
                shape_id: 0,
                screen_width: 1920,
                screen_height: 1080,
                embedded: true,
            });
        });
        expect(toastMock).not.toHaveBeenCalled();
    });
});
