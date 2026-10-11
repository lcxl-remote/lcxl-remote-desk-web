
import { useEffect, useRef, useCallback } from 'react';
import type { RefObject } from 'react';
import type { MouseIntent, Point } from './mobile-interaction';

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

type PressedKey = {
    key_code: number;
    code: string;
};

const DOM_CODE_BY_KEY_CODE: Record<number, string> = {
    8: 'Backspace',
    9: 'Tab',
    13: 'Enter',
    16: 'ShiftLeft',
    17: 'ControlLeft',
    18: 'AltLeft',
    20: 'CapsLock',
    27: 'Escape',
    32: 'Space',
    33: 'PageUp',
    34: 'PageDown',
    35: 'End',
    36: 'Home',
    37: 'ArrowLeft',
    38: 'ArrowUp',
    39: 'ArrowRight',
    40: 'ArrowDown',
    44: 'PrintScreen',
    45: 'Insert',
    46: 'Delete',
    91: 'MetaLeft',
    92: 'MetaRight',
};

function keyCodeToDomCode(keyCode: number): string {
    if (keyCode >= 65 && keyCode <= 90) {
        return `Key${String.fromCharCode(keyCode)}`;
    }
    if (keyCode >= 48 && keyCode <= 57) {
        return `Digit${String.fromCharCode(keyCode)}`;
    }
    if (keyCode >= 112 && keyCode <= 123) {
        return `F${keyCode - 111}`;
    }
    return DOM_CODE_BY_KEY_CODE[keyCode] ?? '';
}

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
            code: keyCodeToDomCode(ev.keyCode),
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
    peerConnection?: RefObject<RTCPeerConnection | null>;
    ignoreInputEvents?: boolean; // When true, don't steal focus or send events (e.g. user is typing in UI)
    isInputAllowed?: () => boolean;
    managedTouches?: boolean;
    mapPosition?: (clientX: number, clientY: number) => Point | null;
    onPointerPosition?: (target: Point) => void;
    onRelease?: () => void;
    remapCtrlToCommand?: boolean; // Windows/Linux controller → macOS host compatibility
};

type PhysicalControlState = {
    left: boolean;
    right: boolean;
};

function isRightControlKey(event: KeyboardEvent): boolean {
    return event.code === "ControlRight" || event.keyCode === 0xA3;
}

function isLeftControlKey(event: KeyboardEvent): boolean {
    return !isRightControlKey(event) && (
        event.code === "ControlLeft"
        || event.keyCode === 17
        || event.keyCode === 0xA2
    );
}

function inferredControlState(event: KeyboardEvent): PhysicalControlState {
    if (isRightControlKey(event)) {
        return { left: false, right: event.ctrlKey };
    }
    return { left: event.ctrlKey, right: false };
}

export function buildPhysicalKeyboardEvent(
    eventType: string,
    event: KeyboardEvent,
    remapCtrlToCommand: boolean,
    controlState: PhysicalControlState = inferredControlState(event),
) {
    const leftControlKey = isLeftControlKey(event);
    const rightControlKey = isRightControlKey(event);

    return {
        event: eventType,
        key: event.key,
        code: event.code,
        key_code: remapCtrlToCommand && leftControlKey
            ? 91
            : remapCtrlToCommand && rightControlKey
                ? 17
                : event.keyCode,
        alt_key: event.altKey,
        ctrl_key: remapCtrlToCommand ? controlState.right : event.ctrlKey,
        shift_key: event.shiftKey,
        // Windows reserves many Win-key chords before the browser sees them.
        // Treat left Ctrl as Command for a macOS host, while right Ctrl remains
        // a literal Control for terminal chords such as Control+C.
        meta_key: event.metaKey || (remapCtrlToCommand && controlState.left),
        repeat: event.repeat,
        location: event.location,
        is_composing: event.isComposing,
    };
}

export function useDeskInput({ videoRef, mouseChannel, keyboardChannel, mouseMoveChannel, isConnected, ignoreInputEvents = false, remapCtrlToCommand = false, managedTouches = false, peerConnection, isInputAllowed, mapPosition, onPointerPosition, onRelease }: UseDeskInputProps) {
    const dimensionsRef = useRef({ width: 0, height: 0 });
    const sequenceNumberRef = useRef(0);
    const pressedKeysRef = useRef<Map<string, PressedKey>>(new Map());
    const physicalControlStateRef = useRef<PhysicalControlState>({ left: false, right: false });
    const pressedButtonsRef = useRef<Set<number>>(new Set());
    // Last cursor position (normalised ratios) reported to the backend.
    // Reused for the synthetic mouseup sent on blur so the release lands
    // where the cursor actually is instead of the surface's top-left.
    const lastPositionRef = useRef({ x: 0, y: 0 });

    const currentRef = useRef({ isConnected, ignoreInputEvents, isInputAllowed, mapPosition, onPointerPosition, onRelease });
    currentRef.current = { isConnected, ignoreInputEvents, isInputAllowed, mapPosition, onPointerPosition, onRelease };

    const inputOpen = useCallback(() => currentRef.current.isConnected && !currentRef.current.ignoreInputEvents
        && (currentRef.current.isInputAllowed?.() ?? true)
        && (!peerConnection || peerConnection.current?.connectionState === 'connected')
        && mouseChannel.current?.readyState === 'open' && keyboardChannel.current?.readyState === 'open'
        && (!mouseMoveChannel || mouseMoveChannel.current?.readyState === 'open'), [peerConnection, mouseChannel, keyboardChannel, mouseMoveChannel]);

    const releaseAllInputs = useCallback(() => {
        const buttons = [...pressedButtonsRef.current];
        const keys = [...pressedKeysRef.current.values()];
        pressedButtonsRef.current.clear();
        pressedKeysRef.current.clear();
        physicalControlStateRef.current = { left: false, right: false };
        for (const button of buttons) {
            try {
                if (mouseChannel.current?.readyState === 'open') mouseChannel.current.send(JSON.stringify({
                    event: 'mouseup', ...lastPositionRef.current, button, buttons: 0,
                    alt_key: false, delta_x: 0, delta_y: 0, sequence_number: 0,
                }));
            } catch { /* Continue releasing the remaining held inputs. */ }
        }
        for (const key of keys) {
            try {
                if (keyboardChannel.current?.readyState === 'open') keyboardChannel.current.send(JSON.stringify({
                    event: 'keyup', key: '', ...key, alt_key: false, ctrl_key: false,
                    shift_key: false, meta_key: false, repeat: false, location: 0, is_composing: false,
                }));
            } catch { /* Host lifecycle cleanup also covers a closed channel. */ }
        }
        currentRef.current.onRelease?.();
    }, [keyboardChannel, mouseChannel]);

    const sendMouseIntent = useCallback((intent: MouseIntent) => {
        if (!inputOpen()) return;
        const channel = intent.event === 'mousemove' ? mouseMoveChannel?.current : mouseChannel.current;
        if (channel?.readyState !== 'open') return;
        channel.send(JSON.stringify({
            event: intent.event === 'position' ? 'mousemove' : intent.event,
            ...intent.target, button: intent.button, buttons: intent.buttons,
            alt_key: false, delta_x: intent.deltaX, delta_y: intent.deltaY,
            sequence_number: intent.event === 'mousemove' ? ++sequenceNumberRef.current : 0,
        }));
        lastPositionRef.current = intent.target;
        if (intent.event === 'mousedown') pressedButtonsRef.current.add(intent.button);
        if (intent.event === 'mouseup') pressedButtonsRef.current.delete(intent.button);
    }, [mouseChannel, mouseMoveChannel, inputOpen]);

    useEffect(() => {
        if (!isConnected) releaseAllInputs();
        const channels = [mouseChannel.current, keyboardChannel.current, mouseMoveChannel?.current];
        for (const channel of channels) channel?.addEventListener?.('close', releaseAllInputs);
        const pc = peerConnection?.current;
        const stateChanged = () => { if (pc?.connectionState !== 'connected') releaseAllInputs(); };
        pc?.addEventListener('connectionstatechange', stateChanged);
        return () => {
            for (const channel of channels) channel?.removeEventListener?.('close', releaseAllInputs);
            pc?.removeEventListener('connectionstatechange', stateChanged);
        };
    }, [isConnected, keyboardChannel, mouseChannel, mouseMoveChannel, peerConnection, releaseAllInputs]);

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
            if (!inputOpen()) return;
            if ((event as MouseEvent & { sourceCapabilities?: { firesTouchEvents: boolean } }).sourceCapabilities?.firesTouchEvents) return;
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

            const mapped = currentRef.current.mapPosition?.(event.clientX, event.clientY);
            if (currentRef.current.mapPosition && !mapped) return;
            const x_ratio = mapped?.x ?? trueX / renderedWidth;
            const y_ratio = mapped?.y ?? trueY / renderedHeight;
            currentRef.current.onPointerPosition?.({ x: x_ratio, y: y_ratio });
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
            if (!inputOpen()) return;
            if (!keyboardChannel.current || keyboardChannel.current.readyState !== "open") {
                return;
            }
            if (remapCtrlToCommand) {
                if (isRightControlKey(event)) {
                    physicalControlStateRef.current.right = eventType === "keydown";
                } else if (isLeftControlKey(event)) {
                    physicalControlStateRef.current.left = eventType === "keydown";
                }
            }
            const keyboardEvent = buildPhysicalKeyboardEvent(
                eventType,
                event,
                remapCtrlToCommand,
                physicalControlStateRef.current,
            );
            keyboardChannel.current.send(JSON.stringify(keyboardEvent));

            const pressedKeyId = keyboardEvent.code || `keyCode:${keyboardEvent.key_code}`;
            if (eventType === "keydown") {
                pressedKeysRef.current.set(pressedKeyId, {
                    key_code: keyboardEvent.key_code,
                    code: keyboardEvent.code,
                });
            } else if (eventType === "keyup") {
                pressedKeysRef.current.delete(pressedKeyId);
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

        const handleVisibilityChange = () => {
            // Switching tabs or minimising (e.g. Cmd+Tab on macOS, which can
            // swallow the key-up of keys held across the switch) does not always
            // fire a window blur. Release everything when the page is hidden so
            // no key — especially a modifier — stays stuck down on the host.
            if (document.hidden) {
                releaseAllInputs();
            }
        };

        element.addEventListener("mousemove", onMouseMove);
        element.addEventListener("mouseup", onMouseUp);
        element.addEventListener("mousedown", onMouseDown);
        element.addEventListener("wheel", onWheel, { passive: false });
        element.addEventListener("contextmenu", onContextMenu);
        element.addEventListener("keydown", onKeyDown);
        element.addEventListener("keyup", onKeyUp);

        if (!managedTouches) {
            element.addEventListener("touchstart", onTouchStart, { passive: false });
            element.addEventListener("touchmove", onTouchMove, { passive: false });
            element.addEventListener("touchend", onTouchEnd, { passive: false });
            element.addEventListener("touchcancel", onTouchCancel, { passive: false });
        }

        element.addEventListener("blur", releaseAllInputs);
        window.addEventListener("blur", releaseAllInputs);
        document.addEventListener("visibilitychange", handleVisibilityChange);

        return () => {
            releaseAllInputs();
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

            element.removeEventListener("blur", releaseAllInputs);
            window.removeEventListener("blur", releaseAllInputs);
            document.removeEventListener("visibilitychange", handleVisibilityChange);
        };
    }, [videoRef, isConnected, mouseChannel, keyboardChannel, mouseMoveChannel, ignoreInputEvents, remapCtrlToCommand, managedTouches, releaseAllInputs, inputOpen]);

    const sendKeyboardEvents = useCallback((events: SyntheticKeyEvent[]) => {
        for (const kbEvent of buildKeyboardEventSequence(events)) {
            const channel = keyboardChannel.current;
            if (!inputOpen() || !channel) break;
            channel.send(JSON.stringify(kbEvent));
            const id = kbEvent.code || `keyCode:${kbEvent.key_code}`;
            if (kbEvent.event === 'keydown') pressedKeysRef.current.set(id, { key_code: kbEvent.key_code, code: kbEvent.code });
            else pressedKeysRef.current.delete(id);
        }
    }, [keyboardChannel, inputOpen]);

    return { sendKeyboardEvents, sendMouseIntent, releaseAllInputs };
}
