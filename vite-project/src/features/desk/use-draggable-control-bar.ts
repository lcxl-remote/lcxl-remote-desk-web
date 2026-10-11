import { useCallback, useEffect, useRef, useState } from 'react';
import type { PointerEvent as ReactPointerEvent, RefObject } from 'react';

type UseDraggableControlBarOptions = {
    controlBarRef: RefObject<HTMLDivElement | null>;
    wrapperRef: RefObject<HTMLDivElement | null>;
};

export function useDraggableControlBar({ controlBarRef, wrapperRef }: UseDraggableControlBarOptions) {
    const [isDragging, setIsDragging] = useState(false);
    const gestureRef = useRef<{ id: number; x: number; y: number; left: number; top: number; target: HTMLElement; moved: boolean } | null>(null);
    const clampPosition = useCallback(() => {
        const bar = controlBarRef.current;
        const wrapper = wrapperRef.current;
        if (!bar || !wrapper || bar.style.transform !== 'none') return;
        const rect = wrapper.getBoundingClientRect();
        const viewport = window.visualViewport;
        const right = viewport ? Math.min(rect.right, viewport.offsetLeft + viewport.width) - rect.left : rect.width;
        const bottom = viewport ? Math.min(rect.bottom, viewport.offsetTop + viewport.height) - rect.top : rect.height;
        bar.style.left = `${Math.max(0, Math.min(parseFloat(bar.style.left) || 0, Math.max(0, right - bar.offsetWidth)))}px`;
        bar.style.top = `${Math.max(0, Math.min(parseFloat(bar.style.top) || 0, Math.max(0, bottom - bar.offsetHeight)))}px`;
    }, [controlBarRef, wrapperRef]);
    const handleDragStart = useCallback((event: ReactPointerEvent) => {
        if (event.button !== 0) return;
        const bar = controlBarRef.current;
        const wrapper = wrapperRef.current;
        if (!bar || !wrapper) return;
        const rect = bar.getBoundingClientRect();
        const bounds = wrapper.getBoundingClientRect();
        gestureRef.current = {
            id: event.pointerId, x: event.clientX, y: event.clientY,
            left: rect.left - bounds.left, top: rect.top - bounds.top,
            target: event.currentTarget as HTMLElement, moved: false,
        };
        (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
    }, [controlBarRef, wrapperRef]);
    useEffect(() => {
        const move = (event: PointerEvent) => {
            const g = gestureRef.current;
            const bar = controlBarRef.current;
            if (!g || !bar || event.pointerId !== g.id) return;
            if (!g.moved && Math.hypot(event.clientX - g.x, event.clientY - g.y) < 4) return;
            g.moved = true;
            g.target.dataset.wasDragged = 'true';
            setIsDragging(true);
            event.preventDefault();
            bar.style.transition = 'none';
            bar.style.transform = 'none';
            bar.style.bottom = 'auto';
            bar.style.left = `${g.left + event.clientX - g.x}px`;
            bar.style.top = `${g.top + event.clientY - g.y}px`;
            clampPosition();
        };
        const end = (event: PointerEvent) => {
            const g = gestureRef.current;
            if (!g || event.pointerId !== g.id) return;
            if (g.target.hasPointerCapture(g.id)) g.target.releasePointerCapture(g.id);
            gestureRef.current = null;
            setIsDragging(false);
            if (controlBarRef.current) controlBarRef.current.style.transition = '';
            clampPosition();
        };
        document.addEventListener('pointermove', move);
        document.addEventListener('pointerup', end);
        document.addEventListener('pointercancel', end);
        return () => {
            document.removeEventListener('pointermove', move);
            document.removeEventListener('pointerup', end);
            document.removeEventListener('pointercancel', end);
        };
    }, [clampPosition, controlBarRef]);
    useEffect(() => {
        const observer = new ResizeObserver(clampPosition);
        if (wrapperRef.current) observer.observe(wrapperRef.current);
        if (controlBarRef.current) observer.observe(controlBarRef.current);
        window.visualViewport?.addEventListener('resize', clampPosition);
        return () => {
            observer.disconnect();
            window.visualViewport?.removeEventListener('resize', clampPosition);
        };
    }, [clampPosition, wrapperRef, controlBarRef]);
    return { handleDragStart, isDragging };
}
