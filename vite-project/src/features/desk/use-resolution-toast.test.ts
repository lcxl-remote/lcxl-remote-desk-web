import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { deskErrorCodeEnum } from "@/services/types";
import {
    DEFAULT_FAILURE_AUTOCLEAR_MS,
    DEFAULT_RESOLUTION_WATCHDOG_MS,
    DEFAULT_SUCCESS_AUTOCLEAR_MS,
    type ResolutionEchoMessage,
    useResolutionToast,
} from "./use-resolution-toast";
import enUS from "@/locales/en-US";

const CHANGE_DISPLAY_SETTINGS = 205;

// Translator backed by the real en-US locale so each test's asserted
// toast text matches what users actually see.
const tr = (key: string) => (enUS as Record<string, string>)[key] ?? key;

function makeApplied(reqId: string, w: number, h: number): ResolutionEchoMessage {
    return {
        signaling_type: CHANGE_DISPLAY_SETTINGS,
        request_id: reqId,
        signaling_data: { width: w, height: h },
        response_state: { error_code: deskErrorCodeEnum.SUCCESS },
    };
}

function makeFailed(reqId: string, code: number, message: string): ResolutionEchoMessage {
    return {
        signaling_type: CHANGE_DISPLAY_SETTINGS,
        request_id: reqId,
        response_state: { error_code: code, message },
    };
}

/** A controllable signaling subscription: a stable `subscribe` plus an
 *  `emit` that synchronously delivers a message to every registered
 *  handler — mirroring the real hook's lossless fan-out. */
function makeSignalingHarness() {
    const handlers = new Set<(m: ResolutionEchoMessage) => void>();
    const subscribe = (h: (m: ResolutionEchoMessage) => void) => {
        handlers.add(h);
        return () => {
            handlers.delete(h);
        };
    };
    const emit = (m: ResolutionEchoMessage) => {
        act(() => {
            handlers.forEach((h) => h(m));
        });
    };
    return { subscribe, emit };
}

function renderToast(initial: { isRTCConnected?: boolean } = {}) {
    const { subscribe, emit } = makeSignalingHarness();
    const view = renderHook(
        (p: { isRTCConnected: boolean }) =>
            useResolutionToast({
                subscribe,
                isRTCConnected: p.isRTCConnected,
                changeDisplaySettingsType: CHANGE_DISPLAY_SETTINGS,
                translate: tr,
            }),
        { initialProps: { isRTCConnected: initial.isRTCConnected ?? true } },
    );
    return { ...view, emit };
}

describe("useResolutionToast", () => {
    beforeEach(() => {
        vi.useFakeTimers();
    });

    afterEach(() => {
        vi.useRealTimers();
    });

    it("starts with no toast", () => {
        const { result } = renderToast();
        expect(result.current.resolutionToast).toBeNull();
    });

    it("registerSent puts the toast into the updating phase with the target dims", () => {
        const { result } = renderToast();
        act(() => result.current.registerSent("r-1", 1920, 1080));
        expect(result.current.resolutionToast).toEqual({
            phase: "updating",
            targetW: 1920,
            targetH: 1080,
            reqId: "r-1",
        });
    });

    it("transitions updating → success on a matching Applied echo and auto-clears", () => {
        const { result, emit } = renderToast();
        act(() => result.current.registerSent("r-1", 1920, 1080));

        emit(makeApplied("r-1", 1920, 1080));
        expect(result.current.resolutionToast).toEqual({
            phase: "success",
            appliedW: 1920,
            appliedH: 1080,
        });

        // Auto-clear after the success window.
        act(() => {
            vi.advanceTimersByTime(DEFAULT_SUCCESS_AUTOCLEAR_MS);
        });
        expect(result.current.resolutionToast).toBeNull();
    });

    it("transitions updating → failed on an error echo with the server message and lingers longer", () => {
        const { result, emit } = renderToast();
        act(() => result.current.registerSent("r-bad", 1280, 720));

        emit(makeFailed("r-bad", deskErrorCodeEnum.FEATURE_UNAVAILABLE, "auto change throttled"));
        expect(result.current.resolutionToast).toEqual({
            phase: "failed",
            reason: "auto change throttled",
        });

        // Failure clears slower than success so the operator has time
        // to read the reason.
        act(() => {
            vi.advanceTimersByTime(DEFAULT_SUCCESS_AUTOCLEAR_MS);
        });
        expect(result.current.resolutionToast).not.toBeNull();

        act(() => {
            vi.advanceTimersByTime(
                DEFAULT_FAILURE_AUTOCLEAR_MS - DEFAULT_SUCCESS_AUTOCLEAR_MS,
            );
        });
        expect(result.current.resolutionToast).toBeNull();
    });

    /**
     * A debounced or stuck old request can still
     * land an echo on the wire after the user moved on. The toast
     * must reflect the latest user intent — old echoes are silently
     * dropped on the floor, not allowed to flicker the new updating
     * toast into a misleading success.
     */
    it("ignores stale echoes whose request id no longer matches the latest registration", () => {
        const { result, emit } = renderToast();
        act(() => result.current.registerSent("r-old", 1920, 1080));
        act(() => result.current.registerSent("r-new", 2560, 1440));

        emit(makeApplied("r-old", 1920, 1080));

        // Should still be updating with the NEW target — the stale
        // echo must not collapse the toast.
        expect(result.current.resolutionToast).toEqual({
            phase: "updating",
            targetW: 2560,
            targetH: 1440,
            reqId: "r-new",
        });
    });

    /**
     * Without a watchdog a lost or never-acked
     * request would freeze the spinner forever. Promote to a
     * timeout-flavoured failed toast at the watchdog deadline.
     */
    it("promotes updating to a timeout failure after the watchdog deadline", () => {
        const { result } = renderToast();
        act(() => result.current.registerSent("r-stuck", 1920, 1080));
        // One ms shy: still updating.
        act(() => {
            vi.advanceTimersByTime(DEFAULT_RESOLUTION_WATCHDOG_MS - 1);
        });
        expect(result.current.resolutionToast?.phase).toBe("updating");

        // Cross the deadline: failed with the timeout reason text.
        act(() => {
            vi.advanceTimersByTime(1);
        });
        expect(result.current.resolutionToast).toEqual({
            phase: "failed",
            reason: "No reply within timeout",
        });
    });

    /**
     * RTC dropping out is an unambiguous signal
     * that no echo is coming. Don't strand the toast on the screen.
     */
    it("clears the toast when isRTCConnected flips to false", () => {
        const { result, rerender } = renderToast({ isRTCConnected: true });
        act(() => result.current.registerSent("r-rtc", 1920, 1080));
        expect(result.current.resolutionToast).not.toBeNull();

        rerender({ isRTCConnected: false });
        expect(result.current.resolutionToast).toBeNull();
    });

    /**
     * Defence-in-depth: even after the RTC clear path resets the
     * latest-id gate, a stray post-reconnect echo bearing the old id
     * must not pop the toast back open.
     */
    it("does not resurrect the toast from a stale echo after RTC reconnect", () => {
        const { result, rerender, emit } = renderToast({ isRTCConnected: true });
        act(() => result.current.registerSent("r-pre-drop", 1920, 1080));
        rerender({ isRTCConnected: false });
        // Reconnect with the same hook instance.
        rerender({ isRTCConnected: true });
        emit(makeApplied("r-pre-drop", 1920, 1080));
        expect(result.current.resolutionToast).toBeNull();
    });

    /**
     * Regression for React error #185 (Maximum update depth exceeded).
     * The `desk-session` parent passes `translate` as an inline arrow
     * `(k) => t(k)` rebuilt on every render. When the hook's
     * effect listed `translate` in its dep array, every parent render
     * re-ran the signaling effect; once a 205 echo had landed and
     * `latestReqIdRef` matched, each re-run produced a fresh
     * `setResolutionToast({ phase: 'success', ... })` object, which
     * re-rendered the parent, which built yet another translate
     * arrow, looping forever and crashing the app the moment 205
     * actually completed. The fix routes translate through a ref so
     * the effect's dep set ignores it. This test pins that
     * invariant: rerendering with a brand-new translate identity
     * after an echo has settled must NOT mutate the toast state or
     * re-invoke the translator.
     */
    it("ignores translate prop identity changes after an echo has settled", () => {
        const t1 = vi.fn((key: string) => tr(key));
        const t2 = vi.fn((key: string) => tr(key));
        const { subscribe, emit } = makeSignalingHarness();
        type Props = {
            isRTCConnected: boolean;
            translate: (k: string) => string;
        };
        const { result, rerender } = renderHook(
            (p: Props) =>
                useResolutionToast({
                    subscribe,
                    isRTCConnected: p.isRTCConnected,
                    changeDisplaySettingsType: CHANGE_DISPLAY_SETTINGS,
                    translate: p.translate,
                }),
            {
                initialProps: {
                    isRTCConnected: true,
                    translate: t1,
                },
            },
        );
        act(() => result.current.registerSent("r-loop", 1280, 720));
        // Land a failed echo with no `message` so the translate
        // fallback path activates (`message ?? translate(...)`).
        // This is the same code path that crashed the app in
        // production once a 205 echo arrived.
        const failedEcho: ResolutionEchoMessage = {
            signaling_type: CHANGE_DISPLAY_SETTINGS,
            request_id: "r-loop",
            response_state: { error_code: deskErrorCodeEnum.FEATURE_UNAVAILABLE },
        };
        emit(failedEcho);
        const t1CallsAfterEcho = t1.mock.calls.length;
        const toastAfterEcho = result.current.resolutionToast;
        expect(toastAfterEcho?.phase).toBe("failed");
        expect(t1CallsAfterEcho).toBeGreaterThanOrEqual(1);

        // Now rerender 5x with a brand-new translate identity each
        // time without re-emitting. Before the fix, listing translate
        // in the effect deps re-fired it on every rerender and re-set
        // the toast, tripping React's #185 in the real app. After the
        // fix the effect's deps no longer include translate, so
        // neither t1 nor t2 gets called again and the toast object
        // stays identical (referential equality).
        for (let i = 0; i < 5; i += 1) {
            rerender({
                isRTCConnected: true,
                translate: t2,
            });
        }
        expect(t1.mock.calls.length).toBe(t1CallsAfterEcho);
        expect(t2).not.toHaveBeenCalled();
        expect(result.current.resolutionToast).toBe(toastAfterEcho);
    });

    /**
     * A new registration mid-flight cancels the previous watchdog.
     * Otherwise the older request's watchdog could fire and flip the
     * new updating toast to "timeout" even though the new request is
     * still legitimately in flight.
     */
    it("re-arms the watchdog on each new registration", () => {
        const { result } = renderToast();
        act(() => result.current.registerSent("r-1", 1920, 1080));
        // Burn most of the first watchdog.
        act(() => {
            vi.advanceTimersByTime(DEFAULT_RESOLUTION_WATCHDOG_MS - 100);
        });
        // New registration with a fresh id; first watchdog should be
        // cancelled.
        act(() => result.current.registerSent("r-2", 1280, 720));
        // If the first watchdog had stayed armed, this 200 ms tick
        // would cross its deadline and flip to failed/timeout.
        act(() => {
            vi.advanceTimersByTime(200);
        });
        expect(result.current.resolutionToast).toEqual({
            phase: "updating",
            targetW: 1280,
            targetH: 720,
            reqId: "r-2",
        });
    });
});
