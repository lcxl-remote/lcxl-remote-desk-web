import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { act, renderHook } from "@testing-library/react";
import {
    DEFAULT_FAILURE_AUTOCLEAR_MS,
    DEFAULT_RESOLUTION_WATCHDOG_MS,
    DEFAULT_SUCCESS_AUTOCLEAR_MS,
    type ResolutionEchoMessage,
    useResolutionToast,
} from "./use-resolution-toast";

const CHANGE_DISPLAY_SETTINGS = 205;

// Identity translator: each test reads back the fallback strings so
// they double as documentation for what shows in the toast.
const tr = (_key: string, fallback: string) => fallback;

function makeApplied(reqId: string, w: number, h: number): ResolutionEchoMessage {
    return {
        signaling_type: CHANGE_DISPLAY_SETTINGS,
        request_id: reqId,
        signaling_data: { width: w, height: h },
        response_state: { error_code: 0 },
    };
}

function makeFailed(reqId: string, code: number, message: string): ResolutionEchoMessage {
    return {
        signaling_type: CHANGE_DISPLAY_SETTINGS,
        request_id: reqId,
        response_state: { error_code: code, message },
    };
}

interface RenderProps {
    lastMessage: ResolutionEchoMessage | null;
    isRTCConnected: boolean;
}

function renderToast(initial: Partial<RenderProps> = {}) {
    const props: RenderProps = {
        lastMessage: initial.lastMessage ?? null,
        isRTCConnected: initial.isRTCConnected ?? true,
    };
    return renderHook(
        (p: RenderProps) =>
            useResolutionToast({
                lastMessage: p.lastMessage,
                isRTCConnected: p.isRTCConnected,
                changeDisplaySettingsType: CHANGE_DISPLAY_SETTINGS,
                translate: tr,
            }),
        { initialProps: props },
    );
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
        const { result, rerender } = renderToast();
        act(() => result.current.registerSent("r-1", 1920, 1080));

        rerender({ lastMessage: makeApplied("r-1", 1920, 1080), isRTCConnected: true });
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
        const { result, rerender } = renderToast();
        act(() => result.current.registerSent("r-bad", 1280, 720));

        rerender({
            lastMessage: makeFailed("r-bad", 7, "auto change throttled"),
            isRTCConnected: true,
        });
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
     * Codex round 1 #3: a debounced or stuck old request can still
     * land an echo on the wire after the user moved on. The toast
     * must reflect the latest user intent — old echoes are silently
     * dropped on the floor, not allowed to flicker the new updating
     * toast into a misleading success.
     */
    it("ignores stale echoes whose request id no longer matches the latest registration", () => {
        const { result, rerender } = renderToast();
        act(() => result.current.registerSent("r-old", 1920, 1080));
        act(() => result.current.registerSent("r-new", 2560, 1440));

        rerender({
            lastMessage: makeApplied("r-old", 1920, 1080),
            isRTCConnected: true,
        });

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
     * Codex round 1 #3: without a watchdog a lost or never-acked
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
     * Codex round 1 #3: RTC dropping out is an unambiguous signal
     * that no echo is coming. Don't strand the toast on the screen.
     */
    it("clears the toast when isRTCConnected flips to false", () => {
        const { result, rerender } = renderToast({ isRTCConnected: true });
        act(() => result.current.registerSent("r-rtc", 1920, 1080));
        expect(result.current.resolutionToast).not.toBeNull();

        rerender({ lastMessage: null, isRTCConnected: false });
        expect(result.current.resolutionToast).toBeNull();
    });

    /**
     * Defence-in-depth: even after the RTC clear path resets the
     * latest-id gate, a stray post-reconnect echo bearing the old id
     * must not pop the toast back open.
     */
    it("does not resurrect the toast from a stale echo after RTC reconnect", () => {
        const { result, rerender } = renderToast({ isRTCConnected: true });
        act(() => result.current.registerSent("r-pre-drop", 1920, 1080));
        rerender({ lastMessage: null, isRTCConnected: false });
        // Reconnect with the same hook instance.
        rerender({ lastMessage: null, isRTCConnected: true });
        rerender({
            lastMessage: makeApplied("r-pre-drop", 1920, 1080),
            isRTCConnected: true,
        });
        expect(result.current.resolutionToast).toBeNull();
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
