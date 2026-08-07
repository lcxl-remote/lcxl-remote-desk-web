
import { useEffect, useRef, useCallback } from 'react';
import type { RefObject } from 'react';

// Windows virtual-key codes of the modifier keys, mapped to the modifier flag
// they drive. Used to reconstruct the modifier state of a synthetic key
// sequence (e.g. the on-screen shortcut menu) so each emitted event carries the
// correct ctrl/shift/alt/meta booleans.
const MODIFIER_KEY_CODES: Record<number, 'shift' | 'ctrl' | 'alt' | 'meta'> = {
    16: 'shift', // VK_SHIFT
    17: 'ctrl',  // VK_CONTROL
    18: 'alt',   // VK_MENU
    91: 'meta',  // VK_LWIN
    92: 'meta',  // VK_RWIN
};

export type SyntheticKeyEvent = { event: 'keydown' | 'keyup'; keyCode: number };

const DOM_DELTA_PIXEL = 0;
const DOM_DELTA_LINE = 1;
const DOM_DELTA_PAGE = 2;
// Browsers that report wheel movement in lines commonly emit three lines for
// one mouse-wheel notch. Forty pixels per line keeps that notch close to the
// roughly 100-120 pixel deltas emitted by pixel-mode browsers.
const WHEEL_LINE_HEIGHT_PX = 40;

/** Convert a DOM wheel delta to the pixel unit used on the input wire. */
export function normalizeWheelDelta(delta: number, deltaMode: number, pageSize: number): number {
    if (!Number.isFinite(delta)) return 0;
    switch (deltaMode) {
        case DOM_DELTA_LINE:
            return delta * WHEEL_LINE_HEIGHT_PX;
        case DOM_DELTA_PAGE:
            return delta * pageSize;
        case DOM_DELTA_PIXEL:
        default:
            return delta;
    }
}

/**
 * Expand a synthetic key sequence (ordered down/up events) into full keyboard
 * payloads, tracking modifier state so every event reports the modifiers held
 * at that point. A chord such as Ctrl+Alt+Del therefore reports ctrl_key/alt_key
 * true on the inner events instead of hard-coding all modifiers to false — the
 * macOS host stamps CGEvent flags from these booleans, so without this the chord
 * would lose its modifiers there.
 */
export function buildKeyboardEventSequence(events: SyntheticKeyEvent[]) {
    const state = { shift: false, ctrl: false, alt: false, meta: false };
    return events.map(ev => {
        const modifier = MODIFIER_KEY_CODES[ev.keyCode];
        if (modifier) {
            // Update before emitting so the modifier key's own event reflects
            // its new state (matching a real KeyboardEvent: a Ctrl keydown
            // already reports ctrlKey === true).
            state[modifier] = ev.event === 'keydown';
        }
        return {
            event: ev.event,
            key: '',
            code: '',
            key_code: ev.keyCode,
            alt_key: state.alt,
            ctrl_key: state.ctrl,
            shift_key: state.shift,
            meta_key: state.meta,
            repeat: false,
            location: 0,
            is_composing: false,
        };
    });
}

type UseDeskInputProps = {
    videoRef: RefObject<HTMLVideoElement | null>;
    mouseChannel: RefObject<RTCDataChannel | null>;
    keyboardChannel: RefObject<RTCDataChannel | null>;
    mouseMoveChannel?: RefObject<RTCDataChannel | null>;
    isConnected: boolean;
    ignoreInputEvents?: boolean; // When true, don't steal focus or send events (e.g. user is typing in UI)
    remapCtrlToCommand?: boolean; // Windows controller → macOS host compatibility
};

function remappedCtrlKeyCode(event: KeyboardEvent): number {
    return event.code === "ControlRight" || event.keyCode === 0xA3 ? 92 : 91;
}

export function buildPhysicalKeyboardEvent(
    eventType: string,
    event: KeyboardEvent,
    remapCtrlToCommand: boolean,
) {
    const isControlKey = event.code === "ControlLeft"
        || event.code === "ControlRight"
        || event.keyCode === 17
        || event.keyCode === 0xA2
        || event.keyCode === 0xA3;

    return {
        event: eventType,
        key: event.key,
        code: event.code,
        key_code: remapCtrlToCommand && isControlKey
            ? remappedCtrlKeyCode(event)
            : event.keyCode,
        alt_key: event.altKey,
        ctrl_key: remapCtrlToCommand ? false : event.ctrlKey,
        shift_key: event.shiftKey,
        // Windows reserves many Win-key chords before the browser sees them.
        // Treat physical Ctrl as Command for a macOS host while retaining any
        // Meta state the browser did manage to deliver.
        meta_key: event.metaKey || (remapCtrlToCommand && event.ctrlKey),
        repeat: event.repeat,
        location: event.location,
        is_composing: event.isComposing,
    };
}

export function useDeskInput({ videoRef, mouseChannel, keyboardChannel, mouseMoveChannel, isConnected, ignoreInputEvents = false, remapCtrlToCommand = false }: UseDeskInputProps) {
    const dimensionsRef = useRef({ width: 0, height: 0 });
    const sequenceNumberRef = useRef(0);
    const pressedKeysRef = useRef<Set<number>>(new Set());
    const pressedButtonsRef = useRef<Set<number>>(new Set());
    // Last cursor position (normalised ratios) reported to the backend.
    // Reused for the synthetic mouseup sent on blur so the release lands
    // where the cursor actually is instead of the surface's top-left.
    const lastPositionRef = useRef({ x: 0, y: 0 });

    useEffect(() => {
        const element = videoRef.current;
        if (!element || !isConnected) return;

        const resizeObserver = new ResizeObserver(entries => {
            for (let entry of entries) {
                dimensionsRef.current = {
                    width: entry.contentRect.width,
                    height: entry.contentRect.height,
                };
            }
        });

        resizeObserver.observe(element);

        return () => {
            resizeObserver.disconnect();
        };
    }, [videoRef, isConnected]);

    useEffect(() => {
        const element = videoRef.current;
        if (!element || !isConnected || ignoreInputEvents) return;

        const handleMouseEvent = (eventType: string, event: MouseEvent | WheelEvent) => {
            const isMouseMove = eventType === "mousemove";
            const channel = isMouseMove ? mouseMoveChannel?.current : mouseChannel.current;

            if (!channel || channel.readyState !== "open") {
                return;
            }
            const dimensions = dimensionsRef.current;
            if (dimensions.width === 0 || dimensions.height === 0) {
                return;
            }

            const videoWidth = element.videoWidth;
            const videoHeight = element.videoHeight;
            if (!videoWidth || !videoHeight) return;

            // calculate rendered video size to ignore the letterboxing
            const scale = Math.min(dimensions.width / videoWidth, dimensions.height / videoHeight);
            const renderedWidth = videoWidth * scale;
            const renderedHeight = videoHeight * scale;

            const offsetX_offset = (dimensions.width - renderedWidth) / 2;
            const offsetY_offset = (dimensions.height - renderedHeight) / 2;

            let trueX = event.offsetX - offsetX_offset;
            let trueY = event.offsetY - offsetY_offset;

            // Clamp out of bounds clicks (on the black bars)
            trueX = Math.max(0, Math.min(trueX, renderedWidth));
            trueY = Math.max(0, Math.min(trueY, renderedHeight));

            const x_ratio = trueX / renderedWidth;
            const y_ratio = trueY / renderedHeight;
            lastPositionRef.current = { x: x_ratio, y: y_ratio };
            let delta_x = 0;
            let delta_y = 0;
            if (eventType === "wheel") {
                const wheelEvent = event as WheelEvent;
                // Read deltaMode before deltaX/deltaY. Some browsers preserve
                // legacy delta values until the unit has been observed.
                const deltaMode = wheelEvent.deltaMode;
                delta_x = normalizeWheelDelta(wheelEvent.deltaX, deltaMode, renderedWidth);
                delta_y = normalizeWheelDelta(wheelEvent.deltaY, deltaMode, renderedHeight);
            }
            const mouseEvent = {
                event: eventType,
                x: x_ratio,
                y: y_ratio,
                button: event.button,
                buttons: event.buttons,
                alt_key: event.altKey,
                delta_x: delta_x,
                delta_y: delta_y,
                sequence_number: isMouseMove ? ++sequenceNumberRef.current : 0,
            };
            channel.send(JSON.stringify(mouseEvent));

            if (eventType === "mousedown") {
                pressedButtonsRef.current.add(event.button);
            } else if (eventType === "mouseup") {
                pressedButtonsRef.current.delete(event.button);
            }
        };

        const handleKeyboardEvent = (eventType: string, event: KeyboardEvent) => {
            if (!keyboardChannel.current || keyboardChannel.current.readyState !== "open") {
                return;
            }
            const keyboardEvent = buildPhysicalKeyboardEvent(
                eventType,
                event,
                remapCtrlToCommand,
            );
            keyboardChannel.current.send(JSON.stringify(keyboardEvent));

            if (eventType === "keydown") {
                pressedKeysRef.current.add(keyboardEvent.key_code);
            } else if (eventType === "keyup") {
                pressedKeysRef.current.delete(keyboardEvent.key_code);
            }
        };

        const onMouseMove = (e: MouseEvent) => handleMouseEvent("mousemove", e);
        const onMouseUp = (e: MouseEvent) => { e.preventDefault(); element.focus(); handleMouseEvent("mouseup", e); };
        const onMouseDown = (e: MouseEvent) => { e.preventDefault(); element.focus(); handleMouseEvent("mousedown", e); };
        const onWheel = (e: WheelEvent) => { e.preventDefault(); e.stopPropagation(); element.focus(); handleMouseEvent("wheel", e); };

        const onContextMenu = (e: MouseEvent) => { e.preventDefault(); element.focus(); };

        const onKeyDown = (e: KeyboardEvent) => { e.preventDefault(); element.focus(); handleKeyboardEvent("keydown", e); };
        const onKeyUp = (e: KeyboardEvent) => { e.preventDefault(); element.focus(); handleKeyboardEvent("keyup", e); };

        const handleTouchEvent = (eventType: string, event: TouchEvent) => {
            const isTouchMove = eventType === "mousemove"; // Because touchmove fires mousemove eventType handler
            const channel = isTouchMove ? mouseMoveChannel?.current : mouseChannel.current;

            if (!channel || channel.readyState !== "open") {
                return;
            }
            if (event.cancelable) {
                event.preventDefault(); // Prevent default touch behaviors like scrolling and pinch-to-zoom
            }
            element.focus();

            const dimensions = dimensionsRef.current;
            if (dimensions.width === 0 || dimensions.height === 0) return;

            const videoWidth = element.videoWidth;
            const videoHeight = element.videoHeight;
            if (!videoWidth || !videoHeight) return;

            const scale = Math.min(dimensions.width / videoWidth, dimensions.height / videoHeight);
            const renderedWidth = videoWidth * scale;
            const renderedHeight = videoHeight * scale;

            const offsetX_offset = (dimensions.width - renderedWidth) / 2;
            const offsetY_offset = (dimensions.height - renderedHeight) / 2;

            const touch = event.touches.length > 0 ? event.touches[0] : event.changedTouches[0];
            if (!touch) return;

            const rect = element.getBoundingClientRect();
            let trueX = (touch.clientX - rect.left) - offsetX_offset;
            let trueY = (touch.clientY - rect.top) - offsetY_offset;

            trueX = Math.max(0, Math.min(trueX, renderedWidth));
            trueY = Math.max(0, Math.min(trueY, renderedHeight));

            const x_ratio = trueX / renderedWidth;
            const y_ratio = trueY / renderedHeight;
            lastPositionRef.current = { x: x_ratio, y: y_ratio };

            const mouseEvent = {
                event: eventType,
                x: x_ratio,
                y: y_ratio,
                button: 0,
                buttons: (eventType === "mousedown" || eventType === "mousemove") ? 1 : 0,
                alt_key: event.altKey || false,
                delta_x: 0,
                delta_y: 0,
                sequence_number: isTouchMove ? ++sequenceNumberRef.current : 0,
            };
            channel.send(JSON.stringify(mouseEvent));
        };

        const onTouchStart = (e: TouchEvent) => handleTouchEvent("mousedown", e);
        const onTouchMove = (e: TouchEvent) => handleTouchEvent("mousemove", e);
        const onTouchEnd = (e: TouchEvent) => handleTouchEvent("mouseup", e);
        const onTouchCancel = (e: TouchEvent) => handleTouchEvent("mouseup", e);

        const handleBlur = () => {
            // Release all pressed mouse buttons
            if (mouseChannel.current && mouseChannel.current.readyState === "open") {
                pressedButtonsRef.current.forEach(button => {
                    const mouseEvent = {
                        event: "mouseup",
                        x: lastPositionRef.current.x,
                        y: lastPositionRef.current.y,
                        button: button,
                        buttons: 0,
                        alt_key: false,
                        delta_x: 0,
                        delta_y: 0,
                        sequence_number: 0,
                    };
                    mouseChannel.current?.send(JSON.stringify(mouseEvent));
                });
            }
            pressedButtonsRef.current.clear();

            // Release all pressed keys
            if (keyboardChannel.current && keyboardChannel.current.readyState === "open") {
                pressedKeysRef.current.forEach(keyCode => {
                    const kbEvent = {
                        event: "keyup",
                        key: "",
                        code: "",
                        key_code: keyCode,
                        alt_key: false,
                        ctrl_key: false,
                        shift_key: false,
                        meta_key: false,
                        repeat: false,
                        location: 0,
                        is_composing: false,
                    };
                    keyboardChannel.current?.send(JSON.stringify(kbEvent));
                });
            }
            pressedKeysRef.current.clear();
        };

        const handleVisibilityChange = () => {
            // Switching tabs or minimising (e.g. Cmd+Tab on macOS, which can
            // swallow the key-up of keys held across the switch) does not always
            // fire a window blur. Release everything when the page is hidden so
            // no key — especially a modifier — stays stuck down on the host.
            if (document.hidden) {
                handleBlur();
            }
        };

        element.addEventListener("mousemove", onMouseMove);
        element.addEventListener("mouseup", onMouseUp);
        element.addEventListener("mousedown", onMouseDown);
        element.addEventListener("wheel", onWheel, { passive: false });
        element.addEventListener("contextmenu", onContextMenu);
        element.addEventListener("keydown", onKeyDown);
        element.addEventListener("keyup", onKeyUp);

        element.addEventListener("touchstart", onTouchStart, { passive: false });
        element.addEventListener("touchmove", onTouchMove, { passive: false });
        element.addEventListener("touchend", onTouchEnd, { passive: false });
        element.addEventListener("touchcancel", onTouchCancel, { passive: false });

        element.addEventListener("blur", handleBlur);
        window.addEventListener("blur", handleBlur);
        document.addEventListener("visibilitychange", handleVisibilityChange);

        return () => {
            element.removeEventListener("mousemove", onMouseMove);
            element.removeEventListener("mouseup", onMouseUp);
            element.removeEventListener("mousedown", onMouseDown);
            element.removeEventListener("wheel", onWheel);
            element.removeEventListener("contextmenu", onContextMenu);
            element.removeEventListener("keydown", onKeyDown);
            element.removeEventListener("keyup", onKeyUp);

            element.removeEventListener("touchstart", onTouchStart);
            element.removeEventListener("touchmove", onTouchMove);
            element.removeEventListener("touchend", onTouchEnd);
            element.removeEventListener("touchcancel", onTouchCancel);

            element.removeEventListener("blur", handleBlur);
            window.removeEventListener("blur", handleBlur);
            document.removeEventListener("visibilitychange", handleVisibilityChange);
        };
    }, [videoRef, isConnected, mouseChannel, keyboardChannel, mouseMoveChannel, ignoreInputEvents, remapCtrlToCommand]);

    const sendKeyboardEvents = useCallback((events: SyntheticKeyEvent[]) => {
        if (!keyboardChannel.current || keyboardChannel.current.readyState !== "open") {
            return;
        }
        for (const kbEvent of buildKeyboardEventSequence(events)) {
            keyboardChannel.current?.send(JSON.stringify(kbEvent));
        }
    }, [keyboardChannel]);

    return { sendKeyboardEvents };
}
