import { useCallback, useEffect, useReducer, useRef, type RefObject } from 'react';
import { MobileInteraction, type MouseIntent, type Point, type TouchPoint } from './mobile-interaction';

export function useMobileDeskInteraction({ videoRef, available, blocked, targetKey, peerConnection, isInputAllowed, mouseChannel, keyboardChannel, mouseMoveChannel }: {
    videoRef: RefObject<HTMLVideoElement | null>;
    available: boolean;
    isInputAllowed: () => boolean;
    blocked: boolean;
    targetKey: string;
    peerConnection: RefObject<RTCPeerConnection | null>;
    mouseChannel: RefObject<RTCDataChannel | null>;
    keyboardChannel: RefObject<RTCDataChannel | null>;
    mouseMoveChannel: RefObject<RTCDataChannel | null>;
}) {
    const surfaceRef = useRef<HTMLDivElement>(null);
    const senderRef = useRef<(intent: MouseIntent) => void>(() => {});
    const modelRef = useRef<MobileInteraction | null>(null);
    if (!modelRef.current) modelRef.current = new MobileInteraction(intent => senderRef.current(intent));
    const model = modelRef.current;
    const [, redraw] = useReducer(n => n + 1, 0);
    const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
    const optionsRef = useRef({ available, blocked, isInputAllowed });
    optionsRef.current = { available, blocked, isInputAllowed };
    const checkAvailability = useCallback(() => optionsRef.current.available && !optionsRef.current.blocked
        && optionsRef.current.isInputAllowed()
        && peerConnection.current?.connectionState === 'connected'
        && [mouseChannel.current, mouseMoveChannel.current, keyboardChannel.current]
            .every(channel => channel?.readyState === 'open'), [peerConnection, mouseChannel, mouseMoveChannel, keyboardChannel]);
    const refresh = useCallback(function refreshState() {
        if (timerRef.current) clearTimeout(timerRef.current);
        timerRef.current = null;
        redraw();
        if (model.nextDeadline !== null) timerRef.current = setTimeout(() => {
            model.setControl(checkAvailability());
            model.advanceTime(performance.now());
            refreshState();
        }, Math.max(0, model.nextDeadline - performance.now()));
    }, [model, checkAvailability]);
    const change = useCallback((action: (model: MobileInteraction) => void) => {
        model.setControl(checkAvailability());
        action(model);
        refresh();
    }, [model, checkAvailability, refresh]);
    const mapPosition = useCallback((clientX: number, clientY: number) => {
        const rect = surfaceRef.current?.getBoundingClientRect();
        return rect ? model.map({ x: clientX - rect.left, y: clientY - rect.top }) : null;
    }, [model]);
    const onPointerPosition = useCallback((point: Point) => {
        model.updatePhysicalPointer(point);
        refresh();
    }, [model, refresh]);
    const onRelease = useCallback(() => {
        // The input adapter already released its snapshot, so discard the gesture without another up.
        model.canControl = false;
        model.cancel();
        model.cursor = null;
        model.markers = [];
        model.dragArmed = false;
        refresh();
    }, [model, refresh]);

    useEffect(() => {
        model.resetTarget();
        const video = videoRef.current;
        const surface = surfaceRef.current;
        if (!video || !surface) return;
        const geometry = () => {
            model.updateGeometry(surface.clientWidth, surface.clientHeight, video.videoWidth, video.videoHeight);
            refresh();
        };
        const observer = new ResizeObserver(geometry);
        observer.observe(surface);
        video.addEventListener('resize', geometry);
        video.addEventListener('loadedmetadata', geometry);
        geometry();
        return () => {
            observer.disconnect();
            video.removeEventListener('resize', geometry);
            video.removeEventListener('loadedmetadata', geometry);
            model.cancel();
            if (timerRef.current) clearTimeout(timerRef.current);
        };
    }, [model, videoRef, targetKey, refresh]);
    useEffect(() => {
        if (blocked) model.resetViewport();
        change(() => {});
        const channels = [mouseChannel.current, mouseMoveChannel.current, keyboardChannel.current];
        const update = () => change(() => {});
        const pc = peerConnection.current;
        pc?.addEventListener('connectionstatechange', update);
        for (const channel of channels) {
            channel?.addEventListener('open', update);
            channel?.addEventListener('close', update);
        }
        return () => {
            pc?.removeEventListener('connectionstatechange', update);
            for (const channel of channels) {
                channel?.removeEventListener('open', update);
                channel?.removeEventListener('close', update);
            }
        };
    }, [available, blocked, change, model, peerConnection, mouseChannel, mouseMoveChannel, keyboardChannel]);
    useEffect(() => {
        const surface = surfaceRef.current;
        if (!surface) return;
        const points = (touches: TouchList): TouchPoint[] => {
            const rect = surface.getBoundingClientRect();
            return Array.from(touches, t => ({ id: t.identifier, x: t.clientX - rect.left, y: t.clientY - rect.top }));
        };
        const handle = (event: TouchEvent) => {
            if (optionsRef.current.blocked) return;
            if (event.cancelable) event.preventDefault();
            videoRef.current?.focus({ preventScroll: true });
            const active = points(event.touches);
            change(m => {
                const now = performance.now();
                if (event.type === 'touchstart') m.begin(active, now);
                else if (event.type === 'touchmove') m.move(active, now);
                else if (event.type === 'touchend') m.end(active, points(event.changedTouches), now);
                else m.cancel(true);
            });
        };
        for (const name of ['touchstart', 'touchmove', 'touchend', 'touchcancel']) surface.addEventListener(name, handle as EventListener, { passive: false });
        return () => {
            for (const name of ['touchstart', 'touchmove', 'touchend', 'touchcancel']) surface.removeEventListener(name, handle as EventListener);
        };
    }, [change, videoRef]);
    return { model, surfaceRef, senderRef, change, mapPosition, onPointerPosition, onRelease };
}
