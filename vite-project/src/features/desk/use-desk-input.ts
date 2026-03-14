
import { useEffect, useRef, useCallback } from 'react';
import type { RefObject } from 'react';

type UseDeskInputProps = {
    videoRef: RefObject<HTMLVideoElement | null>;
    mouseChannel: RefObject<RTCDataChannel | null>;
    keyboardChannel: RefObject<RTCDataChannel | null>;
    mouseMoveChannel?: RefObject<RTCDataChannel | null>;
    isConnected: boolean;
    ignoreInputEvents?: boolean; // When true, don't steal focus or send events (e.g. user is typing in UI)
};

export function useDeskInput({ videoRef, mouseChannel, keyboardChannel, mouseMoveChannel, isConnected, ignoreInputEvents = false }: UseDeskInputProps) {
    const dimensionsRef = useRef({ width: 0, height: 0 });
    const sequenceNumberRef = useRef(0);
    const pressedKeysRef = useRef<Set<number>>(new Set());
    const pressedButtonsRef = useRef<Set<number>>(new Set());

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
            let delta_x = 0;
            let delta_y = 0;
            if (eventType === "wheel") {
                const wheelEvent = event as WheelEvent;
                delta_x = wheelEvent.deltaX;
                delta_y = wheelEvent.deltaY;
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
            const keyboardEvent = {
                event: eventType,
                key: event.key,
                code: event.code,
                key_code: event.keyCode,
                alt_key: event.altKey,
                ctrl_key: event.ctrlKey,
                shift_key: event.shiftKey,
                meta_key: event.metaKey,
                repeat: event.repeat,
                location: event.location,
                is_composing: event.isComposing,
            };
            keyboardChannel.current.send(JSON.stringify(keyboardEvent));

            if (eventType === "keydown") {
                pressedKeysRef.current.add(event.keyCode);
            } else if (eventType === "keyup") {
                pressedKeysRef.current.delete(event.keyCode);
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
                        x: 0,
                        y: 0,
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
        };
    }, [videoRef, isConnected, mouseChannel, keyboardChannel, mouseMoveChannel, ignoreInputEvents]);

    const sendKeyboardEvents = useCallback((events: { event: "keydown" | "keyup", keyCode: number }[]) => {
        if (!keyboardChannel.current || keyboardChannel.current.readyState !== "open") {
            return;
        }
        events.forEach(ev => {
            const kbEvent = {
                event: ev.event,
                key: "",
                code: "",
                key_code: ev.keyCode,
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
    }, [keyboardChannel]);

    return { sendKeyboardEvents };
}
