
import { useEffect, useRef, useCallback } from 'react';
import type { RefObject } from 'react';

type UseDeskInputProps = {
    videoRef: RefObject<HTMLVideoElement | null>;
    mouseChannel: RefObject<RTCDataChannel | null>;
    keyboardChannel: RefObject<RTCDataChannel | null>;
    mouseMoveChannel?: RefObject<RTCDataChannel | null>;
    isConnected: boolean;
};

export function useDeskInput({ videoRef, mouseChannel, keyboardChannel, mouseMoveChannel, isConnected }: UseDeskInputProps) {
    const dimensionsRef = useRef({ width: 0, height: 0 });
    const sequenceNumberRef = useRef(0);

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
        if (!element || !isConnected) return;

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
                event.preventDefault(); // 阻止滚动、拉拽缩放等默认触摸行为
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
        };
    }, [videoRef, isConnected, mouseChannel, keyboardChannel, mouseMoveChannel]);

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
