import { renderHook, act } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { MutableRefObject, RefObject } from "react";
import {
    isAdaptiveResolutionGateOpen,
    normaliseDims,
    useAdaptiveResolution,
    type AdaptiveResolutionGateInputs,
    type UseAdaptiveResolutionParams,
} from "./use-adaptive-resolution";

/**
 * Manual `ResizeObserver` mock — vitest does not provide one and
 * jsdom's polyfill never fires. The single instance is exposed on
 * `mockResizeObserverState` so each test can `fire()` a synthetic
 * rect change at will.
 */
type MockROCallback = (entries: ResizeObserverEntry[]) => void;
const mockResizeObserverState: {
    instances: MockROInstance[];
    fire(rect: { width: number; height: number }): void;
} = {
    instances: [],
    fire(rect) {
        for (const inst of mockResizeObserverState.instances) {
            inst.cb([
                {
                    contentRect: {
                        width: rect.width,
                        height: rect.height,
                    },
                } as unknown as ResizeObserverEntry,
            ]);
        }
    },
};

class MockROInstance {
    cb: MockROCallback;
    constructor(cb: MockROCallback) {
        this.cb = cb;
        mockResizeObserverState.instances.push(this);
    }
    observe(_el: Element) {}
    unobserve(_el: Element) {}
    disconnect() {
        const idx = mockResizeObserverState.instances.indexOf(this);
        if (idx >= 0) {
            mockResizeObserverState.instances.splice(idx, 1);
        }
    }
}

/**
 * Helper for renderHook — builds a `wrapperRef` that resolves to a
 * detached div + a `pendingAutoRequestIds` ref + a mock dispatcher
 * that records every call and returns an id derived from the call
 * count.
 */
function makeHarness(
    overrides: Partial<UseAdaptiveResolutionParams> = {},
): {
    sendCalls: Array<{
        width: number;
        height: number;
        refresh_hz: number;
        auto: true;
    }>;
    pendingIds: MutableRefObject<Set<string>>;
    rerender: (extra: Partial<UseAdaptiveResolutionParams>) => void;
    unmount: () => void;
} {
    const sendCalls: Array<{
        width: number;
        height: number;
        refresh_hz: number;
        auto: true;
    }> = [];
    const pendingIds: MutableRefObject<Set<string>> = { current: new Set() };
    const wrapperRef: RefObject<HTMLDivElement | null> = {
        current: document.createElement("div"),
    };
    let nextId = 1;
    const sendChangeDisplay = (p: {
        width: number;
        height: number;
        refresh_hz: number;
        auto: true;
    }) => {
        sendCalls.push(p);
        return `req-${nextId++}`;
    };

    const initialParams: UseAdaptiveResolutionParams = {
        wrapperRef,
        enabled: true,
        sendChangeDisplay,
        pendingAutoRequestIds: pendingIds,
        ...overrides,
    };

    const { rerender, unmount } = renderHook<void, UseAdaptiveResolutionParams>(
        (params) => useAdaptiveResolution(params),
        { initialProps: initialParams },
    );

    return {
        sendCalls,
        pendingIds,
        rerender: (extra) => rerender({ ...initialParams, ...extra }),
        unmount,
    };
}

describe("normaliseDims", () => {
    /**
     * DPR ≥ 1 with healthy rect: round to nearest pixel × DPR and
     * leave clamp untouched.
     */
    it("applies device pixel ratio", () => {
        expect(normaliseDims(1280, 720, 2)).toEqual({ width: 2560, height: 1440 });
        expect(normaliseDims(1280, 720, 1)).toEqual({ width: 1280, height: 720 });
    });

    /**
     * No 8-alignment after Phase 1.3 — odd numbers must come through
     * untouched. Both clamps still fire: undersized → MIN_DIMENSION,
     * oversized → MAX_DIMENSION.
     */
    it("clamps to [640, 7680] without alignment rounding", () => {
        expect(normaliseDims(1003, 601, 1)).toEqual({ width: 1003, height: 640 });
        expect(normaliseDims(1003, 1003, 1)).toEqual({ width: 1003, height: 1003 });
        expect(normaliseDims(100, 100, 1)).toEqual({ width: 640, height: 640 });
        expect(normaliseDims(9000, 9000, 1)).toEqual({ width: 7680, height: 7680 });
    });

    /**
     * NaN / 0 / Infinity / negative DPR are interpreted as DPR=1 so
     * the hook keeps adapting; the wrapper rect itself is the
     * authoritative source.
     */
    it("falls back to DPR=1 for non-finite DPR", () => {
        expect(normaliseDims(1280, 720, Number.NaN)).toEqual({
            width: 1280,
            height: 720,
        });
        expect(normaliseDims(1280, 720, 0)).toEqual({ width: 1280, height: 720 });
        expect(normaliseDims(1280, 720, Number.POSITIVE_INFINITY)).toEqual({
            width: 1280,
            height: 720,
        });
        expect(normaliseDims(1280, 720, -2)).toEqual({ width: 1280, height: 720 });
    });

    /**
     * Zero / NaN / negative rect dimensions mean the wrapper has not
     * laid out yet — we must NOT send a malformed payload. Returning
     * `null` signals the hook to skip this tick.
     */
    it("returns null for invalid rect", () => {
        expect(normaliseDims(0, 720, 1)).toBeNull();
        expect(normaliseDims(1280, 0, 1)).toBeNull();
        expect(normaliseDims(Number.NaN, 720, 1)).toBeNull();
        expect(normaliseDims(-10, 720, 1)).toBeNull();
        expect(normaliseDims(1280, Number.POSITIVE_INFINITY, 1)).toBeNull();
    });
});

describe("useAdaptiveResolution", () => {
    const originalRO = globalThis.ResizeObserver;
    let originalDpr: number;

    beforeEach(() => {
        vi.useFakeTimers();
        mockResizeObserverState.instances = [];
        (globalThis as any).ResizeObserver = MockROInstance;
        originalDpr = window.devicePixelRatio;
        Object.defineProperty(window, "devicePixelRatio", {
            configurable: true,
            value: 1,
        });
    });

    afterEach(() => {
        vi.useRealTimers();
        (globalThis as any).ResizeObserver = originalRO;
        Object.defineProperty(window, "devicePixelRatio", {
            configurable: true,
            value: originalDpr,
        });
    });

    /**
     * Trailing-edge debounce — five rapid resize events compress into
     * a single send carrying the most recent dimensions.
     */
    it("debounces rapid resizes into a single send", () => {
        const h = makeHarness();
        act(() => {
            mockResizeObserverState.fire({ width: 1000, height: 800 });
            mockResizeObserverState.fire({ width: 1100, height: 800 });
            mockResizeObserverState.fire({ width: 1200, height: 800 });
            mockResizeObserverState.fire({ width: 1300, height: 800 });
            mockResizeObserverState.fire({ width: 1400, height: 800 });
        });
        // Within the debounce window — nothing yet.
        act(() => {
            vi.advanceTimersByTime(4_000);
        });
        expect(h.sendCalls).toHaveLength(0);
        // Complete the debounce.
        act(() => {
            vi.advanceTimersByTime(1_500);
        });
        expect(h.sendCalls).toHaveLength(1);
        expect(h.sendCalls[0].width).toBe(1400);
        expect(h.sendCalls[0].height).toBe(800);
    });

    /**
     * Trailing-edge semantics: a resize 4s into the timer must RESET
     * the countdown, not piggy-back on the original. The next send
     * arrives 5s after the *last* resize, not 5s after the first.
     */
    it("resets the debounce timer on each resize", () => {
        const h = makeHarness();
        act(() => {
            mockResizeObserverState.fire({ width: 1000, height: 800 });
        });
        act(() => {
            vi.advanceTimersByTime(4_000);
        });
        // Second resize at t=4000ms — must restart the 5s window.
        act(() => {
            mockResizeObserverState.fire({ width: 1100, height: 800 });
        });
        act(() => {
            vi.advanceTimersByTime(4_500);
        });
        expect(h.sendCalls).toHaveLength(0);
        // Now t = 4000 + 4500 = 8500ms (still 500ms shy of the second
        // resize's 5s). One more 500ms tick should land the send.
        act(() => {
            vi.advanceTimersByTime(500);
        });
        expect(h.sendCalls).toHaveLength(1);
        expect(h.sendCalls[0].width).toBe(1100);
    });

    /** `debounceMs` prop overrides the 5s default. */
    it("uses provided debounceMs", () => {
        const h = makeHarness({ debounceMs: 1_000 });
        act(() => {
            mockResizeObserverState.fire({ width: 1000, height: 800 });
        });
        act(() => {
            vi.advanceTimersByTime(900);
        });
        expect(h.sendCalls).toHaveLength(0);
        act(() => {
            vi.advanceTimersByTime(150);
        });
        expect(h.sendCalls).toHaveLength(1);
    });

    /** Auto path always sends `refresh_hz: 0` (daemon authoritative). */
    it("always sends refresh_hz: 0", () => {
        const h = makeHarness({ debounceMs: 100 });
        act(() => {
            mockResizeObserverState.fire({ width: 1000, height: 800 });
        });
        act(() => {
            vi.advanceTimersByTime(150);
        });
        expect(h.sendCalls[0].refresh_hz).toBe(0);
        expect(h.sendCalls[0].auto).toBe(true);
    });

    /**
     * After the first send establishes a baseline, a sub-threshold
     * delta must NOT trigger another send. Together with the next
     * test (`min_delta_uses_provided_param`) this pins the threshold
     * is honoured.
     */
    it("skips small delta after first send", () => {
        const h = makeHarness({ debounceMs: 100 });
        act(() => {
            mockResizeObserverState.fire({ width: 1000, height: 800 });
        });
        act(() => {
            vi.advanceTimersByTime(150);
        });
        expect(h.sendCalls).toHaveLength(1);
        // Δ = 8 px on width, far below the default 16 px threshold.
        act(() => {
            mockResizeObserverState.fire({ width: 1008, height: 800 });
        });
        act(() => {
            vi.advanceTimersByTime(150);
        });
        expect(h.sendCalls).toHaveLength(1);
    });

    /**
     * `minDeltaPx` prop wires through — a small threshold accepts the
     * 8 px jump, a large one rejects it.
     */
    it("uses provided minDeltaPx", () => {
        const lo = makeHarness({ debounceMs: 100, minDeltaPx: 4 });
        act(() => {
            mockResizeObserverState.fire({ width: 1000, height: 800 });
        });
        act(() => {
            vi.advanceTimersByTime(150);
        });
        act(() => {
            mockResizeObserverState.fire({ width: 1008, height: 800 });
        });
        act(() => {
            vi.advanceTimersByTime(150);
        });
        expect(lo.sendCalls).toHaveLength(2);

        const hi = makeHarness({ debounceMs: 100, minDeltaPx: 32 });
        act(() => {
            mockResizeObserverState.fire({ width: 1000, height: 800 });
        });
        act(() => {
            vi.advanceTimersByTime(150);
        });
        act(() => {
            mockResizeObserverState.fire({ width: 1008, height: 800 });
        });
        act(() => {
            vi.advanceTimersByTime(150);
        });
        expect(hi.sendCalls).toHaveLength(1);
    });

    /**
     * Each emitted request id must land in the shared pending set so
     * the desk-session response listener can silently drop the echo.
     */
    it("tracks request id into pending set", () => {
        const h = makeHarness({ debounceMs: 100 });
        act(() => {
            mockResizeObserverState.fire({ width: 1000, height: 800 });
        });
        act(() => {
            vi.advanceTimersByTime(150);
        });
        expect(h.pendingIds.current.size).toBe(1);
        expect(h.pendingIds.current.has("req-1")).toBe(true);
    });

    /**
     * Flipping `enabled` to false mid-debounce must tear down the
     * ResizeObserver AND any pending timer — no orphan send after
     * the user toggles the feature off.
     */
    it("stops when enabled flips false", () => {
        const h = makeHarness({ debounceMs: 1_000 });
        act(() => {
            mockResizeObserverState.fire({ width: 1000, height: 800 });
        });
        h.rerender({ enabled: false });
        act(() => {
            vi.advanceTimersByTime(2_000);
        });
        expect(h.sendCalls).toHaveLength(0);
    });

    /**
     * `enabled=false` at mount must NOT instantiate the observer at
     * all. (Compare against `stops_when_enabled_flips_false` which
     * tests the runtime flip.)
     */
    it("does not observe when initially enabled is false", () => {
        makeHarness({ enabled: false });
        expect(mockResizeObserverState.instances).toHaveLength(0);
    });

    /**
     * Regression for the "observer remounts every render" bug:
     * `desk-session` re-renders ~1 Hz on `rtcStats` updates, and each
     * render produced a fresh `sendChangeDisplay` (its `useCallback`
     * transitively depended on `useResolutionToast`'s `registerSent`,
     * whose own deps included an inline `translate` arrow). When the
     * effect listed `sendChangeDisplay` in its dep list, every render
     * tore down the ResizeObserver AND cleared the in-flight debounce
     * timer — so the 5 s trailing-edge timer could never elapse and
     * no 205 ever fired. The fix routes the dispatcher through a ref
     * so the observer survives caller churn. This test verifies BOTH
     * halves: the observer instance is the same after a rerender with
     * a new dispatcher, AND the eventual fire uses the latest
     * dispatcher (not a stale captured one).
     */
    it("survives a sendChangeDisplay reference change without remount, and fires through the latest dispatcher", () => {
        const callsV1: Array<{ width: number; height: number }> = [];
        const callsV2: Array<{ width: number; height: number }> = [];
        const sendV1 = (p: {
            width: number;
            height: number;
            refresh_hz: number;
            auto: true;
        }) => {
            callsV1.push({ width: p.width, height: p.height });
            return "req-v1";
        };
        const sendV2 = (p: {
            width: number;
            height: number;
            refresh_hz: number;
            auto: true;
        }) => {
            callsV2.push({ width: p.width, height: p.height });
            return "req-v2";
        };
        const pendingIds: MutableRefObject<Set<string>> = { current: new Set() };
        const wrapperRef: RefObject<HTMLDivElement | null> = {
            current: document.createElement("div"),
        };
        const initialProps: UseAdaptiveResolutionParams = {
            wrapperRef,
            enabled: true,
            sendChangeDisplay: sendV1,
            pendingAutoRequestIds: pendingIds,
            debounceMs: 1_000,
        };
        const { rerender } = renderHook<void, UseAdaptiveResolutionParams>(
            (params) => useAdaptiveResolution(params),
            { initialProps },
        );
        expect(mockResizeObserverState.instances).toHaveLength(1);
        const observerBefore = mockResizeObserverState.instances[0];

        // Start a debounce window with v1 in place.
        act(() => {
            mockResizeObserverState.fire({ width: 1280, height: 720 });
        });
        // Halfway through the debounce, swap to v2 — simulates the
        // parent re-rendering with a new useCallback identity.
        act(() => {
            vi.advanceTimersByTime(500);
        });
        rerender({ ...initialProps, sendChangeDisplay: sendV2 });

        // Same observer instance — no disconnect/attach churn.
        expect(mockResizeObserverState.instances).toHaveLength(1);
        expect(mockResizeObserverState.instances[0]).toBe(observerBefore);

        // Complete the debounce. v2 must be the dispatcher used, and
        // v1 must NOT have been called at all (a stale-closure
        // regression would surface as a v1 hit).
        act(() => {
            vi.advanceTimersByTime(600);
        });
        expect(callsV1).toHaveLength(0);
        expect(callsV2).toEqual([{ width: 1280, height: 720 }]);
        expect(pendingIds.current.has("req-v2")).toBe(true);
    });
});

describe("isAdaptiveResolutionGateOpen", () => {
    /**
     * Baseline of every axis satisfied — used as the spread base so
     * each negative case only flips the single axis under test.
     */
    const happy: AdaptiveResolutionGateInputs = {
        deskId: "desk-abc",
        isRTCConnected: true,
        virtualDisplayActive: true,
        virtualDisplayDeviceName: "\\\\.\\DISPLAY8",
        selectedVideoDeviceName: "\\\\.\\DISPLAY8",
        adaptiveWebPageResolution: true,
    };

    it("opens the gate when every axis is satisfied", () => {
        expect(isAdaptiveResolutionGateOpen(happy)).toBe(true);
    });

    it("closes when deskId is missing", () => {
        expect(
            isAdaptiveResolutionGateOpen({ ...happy, deskId: null }),
        ).toBe(false);
        expect(
            isAdaptiveResolutionGateOpen({ ...happy, deskId: "" }),
        ).toBe(false);
        expect(
            isAdaptiveResolutionGateOpen({ ...happy, deskId: undefined }),
        ).toBe(false);
    });

    it("closes when WebRTC is not connected", () => {
        expect(
            isAdaptiveResolutionGateOpen({ ...happy, isRTCConnected: false }),
        ).toBe(false);
    });

    it("closes when the daemon reports the IDD as not active", () => {
        expect(
            isAdaptiveResolutionGateOpen({
                ...happy,
                virtualDisplayActive: false,
            }),
        ).toBe(false);
        expect(
            isAdaptiveResolutionGateOpen({
                ...happy,
                virtualDisplayActive: null,
            }),
        ).toBe(false);
    });

    it("closes when the daemon omits the IDD device name", () => {
        expect(
            isAdaptiveResolutionGateOpen({
                ...happy,
                virtualDisplayDeviceName: null,
            }),
        ).toBe(false);
        expect(
            isAdaptiveResolutionGateOpen({
                ...happy,
                virtualDisplayDeviceName: "",
            }),
        ).toBe(false);
    });

    /**
     * The exact-match check is the load-bearing defence: if the user
     * picked a physical monitor, firing 205 would silently change the
     * IDD resolution while WGC keeps capturing the physical screen —
     * the change would be invisible.
     */
    it("closes when the selected display is not the IDD", () => {
        expect(
            isAdaptiveResolutionGateOpen({
                ...happy,
                selectedVideoDeviceName: "\\\\.\\DISPLAY1",
            }),
        ).toBe(false);
        expect(
            isAdaptiveResolutionGateOpen({
                ...happy,
                selectedVideoDeviceName: null,
            }),
        ).toBe(false);
        expect(
            isAdaptiveResolutionGateOpen({
                ...happy,
                selectedVideoDeviceName: "",
            }),
        ).toBe(false);
    });

    it("closes when the user has not ticked the adaptive toggle", () => {
        expect(
            isAdaptiveResolutionGateOpen({
                ...happy,
                adaptiveWebPageResolution: false,
            }),
        ).toBe(false);
        expect(
            isAdaptiveResolutionGateOpen({
                ...happy,
                adaptiveWebPageResolution: null,
            }),
        ).toBe(false);
    });

    /**
     * Regression for the ref-vs-state bug fixed alongside this helper:
     * before the fix the `enabled` expression in `desk-session.tsx`
     * read settings from a ref. Mutating a ref does not trigger a
     * re-render, so a settings change like "select IDD + tick
     * adaptive" could leave `enabled` stuck at the previous bool until
     * some unrelated state forced a re-render. The fix mirrors the
     * settings to React state and routes the boolean through this
     * helper. The test below documents the exact transition by passing
     * the pre- and post-submit input objects through the gate.
     */
    it("transitions false→true when the user moves selection onto the IDD", () => {
        const before: AdaptiveResolutionGateInputs = {
            ...happy,
            // Operator just connected with a physical monitor selected
            // and the adaptive toggle still on (the dialog effect
            // would force it off, but defence-in-depth says the gate
            // must still close on its own).
            selectedVideoDeviceName: "\\\\.\\DISPLAY1",
        };
        expect(isAdaptiveResolutionGateOpen(before)).toBe(false);

        const after: AdaptiveResolutionGateInputs = {
            ...before,
            selectedVideoDeviceName: "\\\\.\\DISPLAY8",
        };
        expect(isAdaptiveResolutionGateOpen(after)).toBe(true);
    });
});
