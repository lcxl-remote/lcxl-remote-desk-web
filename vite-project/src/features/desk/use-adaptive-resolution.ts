import { useEffect, useRef } from "react";
import type { MutableRefObject, RefObject } from "react";

/**
 * Hook params for {@link useAdaptiveResolution}. The browser owns the
 * trailing-edge debounce; the daemon side independently throttles auto
 * requests as defence in depth.
 */
export interface UseAdaptiveResolutionParams {
    /**
     * Wrapper element to observe. The hook tracks `getBoundingClientRect`
     * on this node and treats `width × devicePixelRatio` as the
     * pixel-for-pixel target for the IDD virtual display.
     */
    wrapperRef: RefObject<HTMLDivElement | null>;
    /**
     * Composite gate: callers set this to `true` only when the deskId,
     * RTC connection, daemon-reported `virtual_display_active`, and
     * user-toggled `adaptive_web_page_resolution` are all satisfied.
     * Flipping this `false` mid-debounce immediately cancels the
     * pending timer.
     */
    enabled: boolean;
    /**
     * Dispatcher that posts a ChangeDisplaySettings(205) request and
     * returns the actual `request_id` placed on the wire. The hook
     * records that id in {@link pendingAutoRequestIds} so the
     * desk-session response listener can drop the silent echo.
     */
    sendChangeDisplay: (payload: {
        width: number;
        height: number;
        refresh_hz: number;
        auto: true;
    }) => string;
    /**
     * Shared set of request ids the hook has emitted but not yet seen
     * an echo for. The listener side reads and `.delete()`s on receipt.
     */
    pendingAutoRequestIds: MutableRefObject<Set<string>>;
    /**
     * Trailing-edge debounce window in milliseconds. Defaults to 5000
     * (matches `DEFAULT_ADAPTIVE_DEBOUNCE_MS` on the server). Each
     * resize within the window resets the timer; the send fires only
     * after the wrapper has been stable for this long.
     */
    debounceMs?: number;
    /**
     * Minimum pixel delta on either axis required to (re-)schedule a
     * send. Defaults to 16. Below this both width and height must
     * change less to be skipped — guards against micro-jitter from
     * cursor-driven resize loops.
     */
    minDeltaPx?: number;
}

const DEFAULT_DEBOUNCE_MS = 5_000;
const DEFAULT_MIN_DELTA_PX = 16;

/**
 * Inputs to {@link isAdaptiveResolutionGateOpen}: the union of state
 * pieces the `desk-session` view aggregates before letting the auto-
 * resolution loop fire. Kept as a single explicit object so the call
 * site documents itself and the unit tests can pin each axis.
 */
export interface AdaptiveResolutionGateInputs {
    /** Truthy desk identifier — required by `sendChangeDisplay`. */
    deskId: string | null | undefined;
    /** WebRTC peer connection is up and tracks are flowing. */
    isRTCConnected: boolean;
    /** Daemon supervisor reports the IDD currently has a live handle. */
    virtualDisplayActive: boolean | null | undefined;
    /** GDI name (`\\.\DISPLAYn`) of the attached IDD, as reported by the daemon. */
    virtualDisplayDeviceName: string | null | undefined;
    /** Capture target the user picked in the config dialog. */
    selectedVideoDeviceName: string | null | undefined;
    /** Adaptive toggle from the config dialog form. */
    adaptiveWebPageResolution: boolean | null | undefined;
}

/**
 * Pure gate the `useAdaptiveResolution` hook's `enabled` prop is built
 * from. Returns true only when **every** axis is satisfied:
 *
 *   - `deskId` is real (sendMessage needs a routing target)
 *   - WebRTC is connected (no point adapting an inactive stream)
 *   - daemon reports the IDD as currently attached
 *     (`virtualDisplayActive`) AND surfaces its GDI device name
 *   - the user-selected capture device equals that IDD name — without
 *     this, firing 205 would silently change the IDD resolution while
 *     WGC keeps capturing a physical screen
 *   - the user ticked the adaptive toggle
 *
 * Extracted to its own export so the gate semantics are unit-testable
 * without dragging the whole `DeskSession` mock surface. The previous
 * inline expression in `desk-session.tsx` read `lastSettingsRef.current`
 * — a React ref whose mutation does not trigger a re-render, so the
 * hook's `enabled` could silently miss a settings change. The fix is
 * twofold: mirror the relevant settings to state in the caller, and
 * route the boolean calculation through this helper so a regression
 * test can catch a future revert.
 */
export function isAdaptiveResolutionGateOpen(
    args: AdaptiveResolutionGateInputs,
): boolean {
    return (
        !!args.deskId &&
        args.isRTCConnected &&
        !!args.virtualDisplayActive &&
        !!args.virtualDisplayDeviceName &&
        args.selectedVideoDeviceName === args.virtualDisplayDeviceName &&
        !!args.adaptiveWebPageResolution
    );
}

/** IDD bounds — mirrors `web/virtual-display/src/lib.rs` constants. */
const MIN_DIMENSION = 640;
const MAX_DIMENSION = 7680;

/**
 * Normalise a wrapper CSS rect into the (width, height) the daemon
 * expects on the wire. Multiplies by `devicePixelRatio` for pixel-for-
 * pixel mapping then clamps to `[MIN_DIMENSION, MAX_DIMENSION]`.
 *
 * Returns `null` when the input is unusable: zero / negative / NaN /
 * Infinity rect dimensions usually mean the wrapper has not laid out
 * yet. The hook treats this as "skip this tick" rather than push
 * malformed numbers onto the signaling bus.
 *
 * An invalid `dpr` (NaN / 0 / Infinity / negative) falls back to `1`
 * so the hook continues working under unusual zoom transitions —
 * better to ship a slightly-wrong number than to stop adapting
 * entirely. Exported for direct unit testing.
 */
export function normaliseDims(
    cssW: number,
    cssH: number,
    dpr: number,
): { width: number; height: number } | null {
    const safeDpr = Number.isFinite(dpr) && dpr > 0 ? dpr : 1;
    if (
        !Number.isFinite(cssW) ||
        !Number.isFinite(cssH) ||
        cssW <= 0 ||
        cssH <= 0
    ) {
        return null;
    }
    const clamp = (n: number) =>
        Math.max(MIN_DIMENSION, Math.min(MAX_DIMENSION, Math.round(n * safeDpr)));
    return { width: clamp(cssW), height: clamp(cssH) };
}

/**
 * Drives the auto resolution loop:
 *   1. `ResizeObserver` watches `wrapperRef`.
 *   2. Each callback computes `normaliseDims(rect, devicePixelRatio)`.
 *   3. If the result is null (transient layout) or within `minDeltaPx`
 *      of the last sent value, skip.
 *   4. Otherwise reset a trailing-edge timer and, after `debounceMs`
 *      of stability, fire `sendChangeDisplay({ ..., refresh_hz: 0,
 *      auto: true })`. The daemon substitutes its own authoritative
 *      refresh value.
 *
 * Disabled state and unmount both tear down the observer and any
 * pending timer.
 */
export function useAdaptiveResolution({
    wrapperRef,
    enabled,
    sendChangeDisplay,
    pendingAutoRequestIds,
    debounceMs = DEFAULT_DEBOUNCE_MS,
    minDeltaPx = DEFAULT_MIN_DELTA_PX,
}: UseAdaptiveResolutionParams): void {
    /**
     * `lastSent` is the last (width, height) the hook actually placed
     * on the wire. The min-delta gate compares against this — the goal
     * is to suppress redundant work, not to suppress all updates after
     * the first.
     */
    const lastSentRef = useRef<{ width: number; height: number } | null>(null);
    /** Pending debounce timer handle. */
    const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

    /**
     * Latest-callback-in-ref pattern. The `desk-session` parent
     * re-renders ~1Hz on `rtcStats` updates; every render builds a
     * fresh `sendChangeDisplay` (its `useCallback` transitively depends
     * on `useResolutionToast`'s `registerSent`, whose own deps include
     * the inline `translate` arrow that the parent recreates each
     * render). If the effect below depended on `sendChangeDisplay`
     * directly, the ResizeObserver would be torn down and rebuilt
     * every render, and the trailing-edge debounce timer would be
     * cleared on each rebuild — meaning the 5 s debounce can NEVER
     * complete in practice and no 205 ever fires. Routing through a
     * ref decouples observer lifetime from the caller's render churn
     * while still letting `fire` reach the freshest dispatcher.
     */
    const sendChangeDisplayRef = useRef(sendChangeDisplay);
    sendChangeDisplayRef.current = sendChangeDisplay;

    useEffect(() => {
        if (!enabled) {
            console.info(
                "[adaptive-resolution hook] effect skipped: enabled=false",
            );
            return;
        }
        const wrapper = wrapperRef.current;
        if (!wrapper) {
            console.warn(
                "[adaptive-resolution hook] effect skipped: wrapperRef.current is null",
            );
            return;
        }
        if (typeof ResizeObserver === "undefined") {
            console.warn(
                "[adaptive-resolution hook] effect skipped: ResizeObserver unavailable",
            );
            return;
        }

        const fire = (target: { width: number; height: number }) => {
            timerRef.current = null;
            console.info("[adaptive-resolution hook] fire", {
                width: target.width,
                height: target.height,
            });
            const id = sendChangeDisplayRef.current({
                width: target.width,
                height: target.height,
                refresh_hz: 0,
                auto: true,
            });
            pendingAutoRequestIds.current.add(id);
            lastSentRef.current = target;
        };

        const onResize = (rect: DOMRectReadOnly) => {
            const dpr = window.devicePixelRatio;
            const normalised = normaliseDims(rect.width, rect.height, dpr);
            if (!normalised) {
                console.debug("[adaptive-resolution hook] resize ignored: invalid rect", {
                    cssW: rect.width,
                    cssH: rect.height,
                    dpr,
                });
                return;
            }
            const last = lastSentRef.current;
            const dw = last ? Math.abs(normalised.width - last.width) : Infinity;
            const dh = last ? Math.abs(normalised.height - last.height) : Infinity;
            if (last && dw < minDeltaPx && dh < minDeltaPx) {
                console.debug("[adaptive-resolution hook] resize ignored: below min delta", {
                    normalised,
                    last,
                    dw,
                    dh,
                    minDeltaPx,
                });
                return;
            }
            if (timerRef.current !== null) {
                clearTimeout(timerRef.current);
            }
            console.debug("[adaptive-resolution hook] resize scheduled", {
                cssW: rect.width,
                cssH: rect.height,
                dpr,
                normalised,
                debounceMs,
            });
            timerRef.current = setTimeout(() => fire(normalised), debounceMs);
        };

        const observer = new ResizeObserver((entries) => {
            const entry = entries[entries.length - 1];
            if (entry) {
                onResize(entry.contentRect);
            }
        });
        observer.observe(wrapper);
        console.info("[adaptive-resolution hook] observer attached", {
            wrapperRect: wrapper.getBoundingClientRect(),
            dpr: window.devicePixelRatio,
            debounceMs,
            minDeltaPx,
        });

        return () => {
            observer.disconnect();
            if (timerRef.current !== null) {
                clearTimeout(timerRef.current);
                timerRef.current = null;
            }
            console.info("[adaptive-resolution hook] observer disconnected");
        };
        // `sendChangeDisplay` is intentionally absent — it is reached
        // through `sendChangeDisplayRef` so caller-side render churn
        // does not tear down the observer. `pendingAutoRequestIds`
        // stays in the deps because its identity is stable across
        // renders (the parent holds it in a `useRef`), and React's
        // exhaustive-deps lint expects it.
    }, [enabled, wrapperRef, pendingAutoRequestIds, debounceMs, minDeltaPx]);
}
