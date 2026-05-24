import { useCallback, useEffect, useRef, useState } from "react";
import type { MutableRefObject } from "react";

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
    /** Most recent signaling message observed on the wire. */
    lastMessage: ResolutionEchoMessage | null | undefined;
    /** WebRTC liveness — used to drop a stuck toast when the connection dies. */
    isRTCConnected: boolean;
    /** Numeric `SignalingType::ChangeDisplaySettings` discriminant (205). */
    changeDisplaySettingsType: number;
    /** Caller-supplied i18n hook so the hook has no React-i18next dependency for tests. */
    translate: (key: string, fallback: string) => string;
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
 * - Codex round 1 #3 called out two failure modes the inline draft
 *   couldn't easily cover: an `updating` toast that never clears
 *   when the echo is lost (watchdog) and a stale echo from an old
 *   request id overriding the toast (latest-id gate). Both live
 *   inside this hook.
 */
export function useResolutionToast(
    params: UseResolutionToastParams,
): UseResolutionToastResult {
    const {
        lastMessage,
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
                    reason: translate(
                        "pages.desk.resolutionTimeout",
                        "No reply within timeout",
                    ),
                });
                armAutoClear(failureAutoClearMs);
            }, watchdogMs);
        },
        [
            armAutoClear,
            clearAutoClear,
            clearWatchdog,
            failureAutoClearMs,
            translate,
            watchdogMs,
        ],
    );

    // Drive the state machine off incoming signaling messages.
    useEffect(() => {
        if (!lastMessage) return;
        if (lastMessage.signaling_type !== changeDisplaySettingsType) return;
        const reqId = lastMessage.request_id;
        if (!reqId || reqId !== latestReqIdRef.current) {
            // Stale echo from a request that was already superseded
            // by a newer registration. Drop it on the floor — the
            // toast must reflect the latest user intent, not the
            // resolution of a vacated request.
            return;
        }
        clearWatchdog();
        const errorCode = lastMessage.response_state?.error_code ?? 0;
        if (errorCode === 0) {
            const data = lastMessage.signaling_data ?? {};
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
                    lastMessage.response_state?.message ??
                    translate(
                        "pages.desk.resolutionFailed",
                        "Update failed",
                    ),
            });
            armAutoClear(failureAutoClearMs);
        }
    }, [
        lastMessage,
        armAutoClear,
        clearWatchdog,
        changeDisplaySettingsType,
        failureAutoClearMs,
        successAutoClearMs,
        translate,
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
