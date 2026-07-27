import { useCallback, useEffect, useRef, useState } from "react";
import type { MutableRefObject } from "react";
import { deskErrorCodeEnum } from "@/services/types";

/**
 * Toast phases driven by the adaptive-resolution round-trip.
 *
 * `updating` is entered the moment the browser hook fires a 205; it
 * stays on screen until either a matching response arrives or the
 * watchdog elapses, whichever comes first. `success` / `failed` are
 * the two terminal phases (auto-cleared by a follow-up timer the
 * caller owns).
 */
export type ResolutionToast =
    | { phase: "updating"; targetW: number; targetH: number; reqId: string }
    | { phase: "success"; appliedW: number; appliedH: number }
    | { phase: "failed"; reason: string }
    | null;

export interface ResolutionEchoData {
    /** Width the daemon actually applied. May differ from the request when the driver snaps to the closest supported mode. */
    width?: number;
    /** Height the daemon actually applied. */
    height?: number;
}

/**
 * Minimal slice of `SignalingModel` the hook needs to react to a
 * 205 echo. Keeping this local avoids dragging the whole signaling
 * type tree into the test fixtures.
 */
export interface ResolutionEchoMessage {
    signaling_type: number;
    request_id?: string;
    signaling_data?: ResolutionEchoData | null;
    response_state?: { error_code: number; message?: string } | null;
}

export interface UseResolutionToastParams {
    /**
     * Subscribe to inbound signaling messages; returns an unsubscribe
     * function. The hook keeps its own narrow `ResolutionEchoMessage`
     * shape so its tests stay decoupled from the full signaling type
     * tree (the real signaling message is structurally compatible).
     */
    subscribe: (handler: (msg: ResolutionEchoMessage) => void) => () => void;
    /** WebRTC liveness — used to drop a stuck toast when the connection dies. */
    isRTCConnected: boolean;
    /** Numeric `SignalingType::ChangeDisplaySettings` discriminant (205). */
    changeDisplaySettingsType: number;
    /** Caller-supplied i18n hook so the hook has no React-i18next dependency for tests. */
    translate: (key: string) => string;
    /**
     * Watchdog window for the `updating` phase. If no matching echo
     * arrives within this many ms, the toast transitions to
     * `failed{reason: t('resolutionTimeout')}` and auto-clears 4 s
     * later. Default 15_000 ms — long enough to cover an honest IDD
     * mode change (driver round-trip + WGC restart can easily take a
     * few seconds) while still surfacing a stuck request before the
     * user gives up and reloads.
     */
    watchdogMs?: number;
    /** Auto-clear delay after a successful echo. Default 2 s. */
    successAutoClearMs?: number;
    /** Auto-clear delay after a failure / timeout. Default 4 s so the operator can read the reason. */
    failureAutoClearMs?: number;
}

export const DEFAULT_RESOLUTION_WATCHDOG_MS = 15_000;
export const DEFAULT_SUCCESS_AUTOCLEAR_MS = 2_000;
export const DEFAULT_FAILURE_AUTOCLEAR_MS = 4_000;

export interface UseResolutionToastResult {
    /** Current toast phase, or `null` when nothing should be shown. */
    resolutionToast: ResolutionToast;
    /**
     * Call after `sendMessage` returns the wire request id. Records
     * the id as "the only one allowed to transition the toast" and
     * arms the watchdog. Subsequent calls overwrite the previous id —
     * stale echoes for the prior request will be ignored.
     */
    registerSent: (reqId: string, targetW: number, targetH: number) => void;
    /**
     * Test-only escape hatch: track of pending ids the parent
     * component might still rely on. We do NOT expose this in
     * production usage — the parent's existing
     * `pendingAutoRequestIdsRef` set is the cross-component contract;
     * `latestReqIdRef` is the toast-internal stricter version.
     */
    latestReqIdRef: MutableRefObject<string | null>;
}

/**
 * State machine for the adaptive-resolution status toast.
 *
 * Why a dedicated hook and not inline state in `DeskSession`:
 * - The parent already juggles dozens of effects/refs; the toast
 *   logic is self-contained and can be unit-tested with fake timers
 *   in isolation, without spinning up the full WebRTC + signaling
 *   mock surface.
 * - The state machine covers two failure modes that inline state
 *   cannot handle cleanly: an `updating` toast that never clears
 *   when the echo is lost (watchdog) and a stale echo from an old
 *   request id overriding the toast (latest-id gate). Both live
 *   inside this hook.
 */
export function useResolutionToast(
    params: UseResolutionToastParams,
): UseResolutionToastResult {
    const {
        subscribe,
        isRTCConnected,
        changeDisplaySettingsType,
        translate,
        watchdogMs = DEFAULT_RESOLUTION_WATCHDOG_MS,
        successAutoClearMs = DEFAULT_SUCCESS_AUTOCLEAR_MS,
        failureAutoClearMs = DEFAULT_FAILURE_AUTOCLEAR_MS,
    } = params;

    const [resolutionToast, setResolutionToast] = useState<ResolutionToast>(null);

    // Single-flight gate: only the most recent registered id is
    // allowed to push the toast off `updating`. Any earlier echo —
    // typically a debounced auto-request that was already obsoleted
    // by a newer registration — is silently dropped.
    const latestReqIdRef = useRef<string | null>(null);

    /**
     * Latest-callback-in-ref for `translate`. The desk-session caller
     * builds the translator as an inline arrow `(k) => t(k)` on
     * every render, so the prop's identity changes ~1 Hz (rtcStats
     * setState pulse). If the effects below listed `translate`
     * directly in their deps, every render would re-subscribe them; the
     * signaling handler would then `setResolutionToast(success)`
     * (a fresh object reference) on each tick, triggering yet another
     * render — React error #185 (Maximum update depth exceeded) the
     * moment a 205 echo lands. Routing through a ref keeps the
     * effects stable while still letting them read the most recent
     * translator at call time.
     */
    const translateRef = useRef(translate);
    translateRef.current = translate;

    // Two independent timer slots:
    //   - `autoClearRef` for the `success` / `failed` fade-out
    //   - `watchdogRef` for the `updating → failed{timeout}` fallback
    // Holding them in separate refs makes it impossible to
    // accidentally cancel the auto-clear when we re-arm the
    // watchdog, or vice versa.
    const autoClearRef = useRef<ReturnType<typeof setTimeout> | null>(null);
    const watchdogRef = useRef<ReturnType<typeof setTimeout> | null>(null);

    const clearAutoClear = useCallback(() => {
        if (autoClearRef.current !== null) {
            clearTimeout(autoClearRef.current);
            autoClearRef.current = null;
        }
    }, []);

    const clearWatchdog = useCallback(() => {
        if (watchdogRef.current !== null) {
            clearTimeout(watchdogRef.current);
            watchdogRef.current = null;
        }
    }, []);

    const armAutoClear = useCallback(
        (ms: number) => {
            clearAutoClear();
            autoClearRef.current = setTimeout(() => {
                autoClearRef.current = null;
                setResolutionToast(null);
            }, ms);
        },
        [clearAutoClear],
    );

    const registerSent = useCallback(
        (reqId: string, targetW: number, targetH: number) => {
            latestReqIdRef.current = reqId;
            // A fresh request supersedes whatever was on screen — kill
            // any in-flight auto-clear so the success-of-the-old-one
            // does not blink off the new updating toast.
            clearAutoClear();
            clearWatchdog();
            setResolutionToast({
                phase: "updating",
                targetW,
                targetH,
                reqId,
            });
            watchdogRef.current = setTimeout(() => {
                watchdogRef.current = null;
                setResolutionToast({
                    phase: "failed",
                    reason: translateRef.current("pages.desk.resolutionTimeout"),
                });
                armAutoClear(failureAutoClearMs);
            }, watchdogMs);
        },
        // `translate` deliberately omitted — read via `translateRef`
        // so caller-side re-renders don't churn this callback's
        // identity and ripple through the downstream effects.
        [
            armAutoClear,
            clearAutoClear,
            clearWatchdog,
            failureAutoClearMs,
            watchdogMs,
        ],
    );

    // Drive the state machine off incoming signaling messages.
    useEffect(() => {
        const handle = (message: ResolutionEchoMessage) => {
            if (message.signaling_type !== changeDisplaySettingsType) return;
            const reqId = message.request_id;
            if (!reqId || reqId !== latestReqIdRef.current) {
                // Stale echo from a request that was already superseded
                // by a newer registration. Drop it on the floor — the
                // toast must reflect the latest user intent, not the
                // resolution of a vacated request.
                return;
            }
            clearWatchdog();
            const errorCode =
                message.response_state?.error_code ?? deskErrorCodeEnum.SUCCESS;
            if (errorCode === deskErrorCodeEnum.SUCCESS) {
                const data = message.signaling_data ?? {};
                setResolutionToast({
                    phase: "success",
                    appliedW: data.width ?? 0,
                    appliedH: data.height ?? 0,
                });
                armAutoClear(successAutoClearMs);
            } else {
                setResolutionToast({
                    phase: "failed",
                    reason:
                        message.response_state?.message ??
                        translateRef.current("pages.desk.resolutionFailed"),
                });
                armAutoClear(failureAutoClearMs);
            }
        };
        return subscribe(handle);
        // `translate` deliberately omitted — see translateRef block
        // above. Including it here would make the effect re-run on
        // every parent render and infinite-loop after a 205 echo.
    }, [
        subscribe,
        armAutoClear,
        clearWatchdog,
        changeDisplaySettingsType,
        failureAutoClearMs,
        successAutoClearMs,
    ]);

    // RTC drop: throw out a stuck toast — the worker is gone, no
    // echo is coming. Also reset the latest-id gate so a stale
    // post-reconnect echo cannot transition the empty toast.
    useEffect(() => {
        if (!isRTCConnected) {
            clearAutoClear();
            clearWatchdog();
            latestReqIdRef.current = null;
            setResolutionToast(null);
        }
    }, [isRTCConnected, clearAutoClear, clearWatchdog]);

    // Unmount cleanup so a long-armed timer cannot fire into a
    // disposed component (React 18 strict-mode double-mount in dev
    // makes this an easy regression to miss).
    useEffect(
        () => () => {
            clearAutoClear();
            clearWatchdog();
        },
        [clearAutoClear, clearWatchdog],
    );

    return { resolutionToast, registerSent, latestReqIdRef };
}
