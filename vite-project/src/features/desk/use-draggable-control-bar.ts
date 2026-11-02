import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import type { CSSProperties, PointerEvent as ReactPointerEvent, RefObject } from 'react';
import { ToolbarDrag, floatingBounds, floatingPlacement, isVerticalToolbar, readToolbarPreferences, toolbarEdge, toolbarLayout, writeToolbarPreferences } from './toolbar-layout';
import type { ToolbarPlacement, ToolbarPreferences, ToolbarRect, ToolbarSize } from './toolbar-layout';

function browserStorage(): Storage | null {
    try { return window.localStorage; } catch { return null; }
}
export function useDraggableControlBar({ controlBarRef, wrapperRef, onGrab, onLayoutChange }: {
    controlBarRef: RefObject<HTMLDivElement | null>;
    wrapperRef: RefObject<HTMLDivElement | null>;
    onGrab?: () => void;
    onLayoutChange?: () => void;
}) {
    const [preferences, setPreferences] = useState(() => readToolbarPreferences(browserStorage()));
    const [bounds, setBounds] = useState<ToolbarRect>({ x: 0, y: 0, width: 0, height: 0 });
    const [size, setSize] = useState<ToolbarSize>({ width: 54, height: 54 });
    const [thickness, setThickness] = useState(54);
    const [isDragging, setIsDragging] = useState(false);
    const [isInteracting, setIsInteracting] = useState(false);
    const interactionActive = useRef(false);
    const [preview, setPreview] = useState<ToolbarRect | null>(null);
    const [candidate, setCandidate] = useState<ReturnType<typeof toolbarEdge>>(null);
    const gesture = useRef<{ id: number; model: ToolbarDrag; target: HTMLElement } | null>(null);
    const pendingDrop = useRef<ToolbarDrag | null>(null);
    const pointers = useRef(new Set<number>());
    const callbacks = useRef({ onGrab, onLayoutChange });
    callbacks.current = { onGrab, onLayoutChange };
    const current = useRef({ preferences, bounds, size, thickness });
    current.current = { preferences, bounds, size, thickness };
    const edge = toolbarEdge(preferences.placement);
    const vertical = isVerticalToolbar(edge);
    const layout = toolbarLayout(bounds, preferences.placement, size, thickness);
    const save = useCallback((value: ToolbarPreferences) => {
        current.current.preferences = value;
        setPreferences(value);
        writeToolbarPreferences(browserStorage(), value);
    }, []);
    useLayoutEffect(() => {
        const drop = pendingDrop.current, bar = controlBarRef.current;
        if (!drop || !bar) return;
        pendingDrop.current = null;
        save({ ...current.current.preferences, placement: drop.finish({ width: bar.offsetWidth, height: bar.offsetHeight }) });
    }, [preferences.placement, controlBarRef, save]);
    const cancel = useCallback((suppressClick = true) => {
        const g = gesture.current;
        gesture.current = null;
        if (g && suppressClick) g.target.dataset.wasDragged = 'true';
        if (g?.target.hasPointerCapture?.(g.id)) g.target.releasePointerCapture(g.id);
        setIsDragging(false); setPreview(null); setCandidate(null);
    }, []);
    const measureBounds = useCallback(() => {
        const root = wrapperRef.current;
        if (!root) return { x: 0, y: 0, width: 0, height: 0 };
        const rect = root.getBoundingClientRect();
        const viewport = window.visualViewport;
        const style = getComputedStyle(root);
        const inset = (side: string) => parseFloat(style.getPropertyValue(`--desk-safe-${side}`)) || 0;
        const left = Math.max(rect.left, viewport?.offsetLeft ?? rect.left) + inset('left');
        const top = Math.max(rect.top, viewport?.offsetTop ?? rect.top) + inset('top');
        const right = Math.min(rect.right, viewport ? viewport.offsetLeft + viewport.width : rect.right) - inset('right');
        const bottom = Math.min(rect.bottom, viewport ? viewport.offsetTop + viewport.height : rect.bottom) - inset('bottom');
        return { x: left - rect.left, y: top - rect.top, width: Math.max(0, right - left), height: Math.max(0, bottom - top) };
    }, [wrapperRef]);
    useLayoutEffect(() => {
        const update = () => {
            const next = measureBounds();
            const old = current.current.bounds;
            if (next.x !== old.x || next.y !== old.y || next.width !== old.width || next.height !== old.height) {
                cancel(); callbacks.current.onLayoutChange?.(); current.current.bounds = next; setBounds(next);
            }
            const bar = controlBarRef.current;
            if (bar && bar.offsetWidth > 0 && bar.offsetHeight > 0) {
                setThickness(vertical ? bar.offsetWidth : bar.offsetHeight);
                setSize(previous => previous.width === bar.offsetWidth && previous.height === bar.offsetHeight ? previous
                    : { width: bar.offsetWidth, height: bar.offsetHeight });
            }
        };
        const observer = new ResizeObserver(update);
        if (wrapperRef.current) observer.observe(wrapperRef.current);
        if (controlBarRef.current) observer.observe(controlBarRef.current);
        window.visualViewport?.addEventListener('resize', update);
        window.visualViewport?.addEventListener('scroll', update);
        window.addEventListener('resize', update);
        update();
        return () => {
            observer.disconnect(); window.removeEventListener('resize', update);
            window.visualViewport?.removeEventListener('resize', update);
            window.visualViewport?.removeEventListener('scroll', update);
        };
    }, [cancel, measureBounds, wrapperRef, controlBarRef, vertical]);
    const handleDragStart = useCallback((event: ReactPointerEvent) => {
        if (event.button !== 0 || event.isPrimary === false || gesture.current) return;
        const root = wrapperRef.current, bar = controlBarRef.current;
        if (!root || !bar) return;
        event.preventDefault();
        callbacks.current.onGrab?.();
        const r = root.getBoundingClientRect(), b = bar.getBoundingClientRect();
        const safe = measureBounds();
        const model = new ToolbarDrag(current.current.preferences.placement, safe,
            { x: b.left - r.left, y: b.top - r.top, width: b.width, height: b.height },
            { x: event.clientX - r.left, y: event.clientY - r.top }, event.pointerType === 'touch', current.current.thickness);
        gesture.current = { id: event.pointerId, model, target: event.currentTarget as HTMLElement };
        pointers.current.add(event.pointerId); interactionActive.current = true; setIsInteracting(true);
        (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
    }, [wrapperRef, controlBarRef, measureBounds]);
    useEffect(() => {
        const down = (event: PointerEvent) => {
            pointers.current.add(event.pointerId);
            if (gesture.current && event.pointerId !== gesture.current.id) cancel();
        };
        const move = (event: PointerEvent) => {
            const g = gesture.current, root = wrapperRef.current;
            if (!g || !root || event.pointerId !== g.id) return;
            const r = root.getBoundingClientRect();
            g.model.move({ x: event.clientX - r.left, y: event.clientY - r.top });
            if (!g.model.moved) return;
            event.preventDefault(); g.target.dataset.wasDragged = 'true';
            setIsDragging(true); setPreview({ ...g.model.preview }); setCandidate(g.model.candidate);
        };
        const end = (event: PointerEvent) => {
            const g = gesture.current;
            if (g && event.pointerId === g.id) {
                const root = wrapperRef.current;
                if (event.type === 'pointerup' && root) {
                    const rect = root.getBoundingClientRect();
                    g.model.move({ x: event.clientX - rect.left, y: event.clientY - rect.top });
                }
                if (event.type === 'pointerup' && g.model.moved) {
                    g.target.dataset.wasDragged = 'true';
                    callbacks.current.onLayoutChange?.();
                    const s = current.current.size;
                    const floatingSize = { width: current.current.preferences.expanded ? Math.max(s.width, s.height) : g.model.thickness, height: g.model.thickness };
                    const next = { ...current.current.preferences, placement: g.model.finish(g.model.candidate ? s : floatingSize) };
                    pendingDrop.current = g.model;
                    current.current.preferences = next;
                    setPreferences(next);
                }
                cancel(event.type !== 'pointerup' || g.model.moved);
            }
            pointers.current.delete(event.pointerId);
            if (!pointers.current.size) { interactionActive.current = false; setIsInteracting(false); }
        };
        const lost = (event: PointerEvent) => { if (gesture.current?.id === event.pointerId) cancel(); };
        const blur = () => { cancel(); pointers.current.clear(); interactionActive.current = false; setIsInteracting(false); };
        const hidden = () => { if (document.hidden) blur(); };
        document.addEventListener('pointerdown', down, true);
        document.addEventListener('pointermove', move, { passive: false });
        document.addEventListener('pointerup', end); document.addEventListener('pointercancel', end);
        document.addEventListener('lostpointercapture', lost);
        document.addEventListener('visibilitychange', hidden); window.addEventListener('blur', blur);
        return () => {
            document.removeEventListener('pointerdown', down, true); document.removeEventListener('pointermove', move);
            document.removeEventListener('pointerup', end); document.removeEventListener('pointercancel', end);
            document.removeEventListener('lostpointercapture', lost); document.removeEventListener('visibilitychange', hidden);
            window.removeEventListener('blur', blur);
        };
    }, [cancel, save, wrapperRef]);
    const setPlacement = (placement: ToolbarPlacement) => {
        cancel(); callbacks.current.onLayoutChange?.(); save({ ...current.current.preferences, placement });
    };
    const setExpanded = (expanded: boolean) => save({ ...current.current.preferences, expanded });
    const bar = preview ?? layout.bar;
    const controlBarStyle: CSSProperties = {
        left: bar.x, top: bar.y, bottom: 'auto', transform: 'none',
        maxWidth: bounds.width > 0 ? Math.max(0, bounds.width - (edge ? 0 : 32)) : undefined, maxHeight: bounds.height > 0 ? Math.max(0, bounds.height - (edge ? 0 : 32)) : undefined,
    };
    if (!preview && preferences.placement.kind === 'floating') {
        const area = floatingBounds(bounds), { x, y } = preferences.placement;
        // Resolve against the live CSS box, including frames before ResizeObserver reports its size.
        controlBarStyle.left = area.x + x * area.width;
        controlBarStyle.top = area.y + y * area.height;
        controlBarStyle.transform = `translate(${-x * 100}%, ${-y * 100}%)`;
        controlBarStyle.maxWidth = bounds.width > 0 ? area.width : undefined;
        controlBarStyle.maxHeight = bounds.height > 0 ? area.height : undefined;
    }
    const isInputSuspended = useCallback(() => {
        const c = current.current;
        const v = toolbarLayout(c.bounds, c.preferences.placement, c.size, c.thickness).video;
        return interactionActive.current || v.width <= 0 || v.height <= 0;
    }, []);
    return { handleDragStart, isDragging, isInteracting, isSettling: pendingDrop.current !== null, isInputSuspended, cancel, preferences, setExpanded, setPlacement,
        edge, vertical, bounds, layout, controlBarStyle,
        viewportStyle: { left: layout.video.x, top: layout.video.y, width: layout.video.width, height: layout.video.height } as CSSProperties,
        previewStyle: candidate ? toolbarLayout(bounds, { kind: 'docked', edge: candidate, along: .5 }, size, thickness).rail : null,
        float: () => setPlacement(floatingPlacement(layout.bar, bounds)),
    };
}
