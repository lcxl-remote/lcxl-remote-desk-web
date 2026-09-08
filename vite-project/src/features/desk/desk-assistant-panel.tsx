import { useLayoutEffect, useRef, useState, type PointerEvent, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { GripHorizontal, Grip, X } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { AiAssistantIcon } from '@/components/ai-assistant-icon';

type Bounds = { width: number; height: number };
type Geometry = Bounds & { x: number; y: number };

export function constrainAssistantPanel(panel: Geometry, bounds: Bounds): Geometry {
    const width = Math.min(Math.max(panel.width, 320), Math.max(0, bounds.width));
    const height = Math.min(Math.max(panel.height, 280), Math.max(0, bounds.height));
    return { width, height, x: Math.max(0, Math.min(panel.x, bounds.width - width)),
        y: Math.max(0, Math.min(panel.y, bounds.height - height)) };
}

/** Hide without unmounting: the shared workspace continues receiving its run events. */
export function DeskAssistantPanel({ open, onClose, onFocus, children }: {
    open: boolean;
    onClose: () => void;
    onFocus: () => void;
    children: ReactNode;
}) {
    const { t } = useTranslation();
    const ref = useRef<HTMLElement>(null);
    const bounds = useRef<Bounds>({ width: 0, height: 0 });
    const [geometry, setGeometry] = useState<Geometry>({ x: 16, y: 16, width: 480, height: 680 });
    const gesture = useRef<{ x: number; y: number; geometry: Geometry; resize: boolean } | null>(null);
    const closeRef = useRef<HTMLButtonElement>(null);
    useLayoutEffect(() => {
        const parent = ref.current?.parentElement;
        if (!parent) return;
        const update = () => {
            bounds.current = { width: parent.clientWidth, height: parent.clientHeight };
            setGeometry(previous => constrainAssistantPanel(previous, bounds.current));
        };
        update();
        const observer = new ResizeObserver(update);
        observer.observe(parent);
        return () => observer.disconnect();
    }, []);
    useLayoutEffect(() => {
        if (open) closeRef.current?.focus();
    }, [open]);
    function start(event: PointerEvent<HTMLElement>, resize: boolean) {
        if (event.button !== 0) return;
        event.preventDefault();
        event.currentTarget.setPointerCapture(event.pointerId);
        gesture.current = { x: event.clientX, y: event.clientY, geometry, resize };
    }
    function move(event: PointerEvent<HTMLElement>) {
        const current = gesture.current;
        if (!current) return;
        const dx = event.clientX - current.x;
        const dy = event.clientY - current.y;
        setGeometry(constrainAssistantPanel(current.resize
            ? { ...current.geometry, width: current.geometry.width + dx, height: current.geometry.height + dy }
            : { ...current.geometry, x: current.geometry.x + dx, y: current.geometry.y + dy }, bounds.current));
    }
    const stop = () => { gesture.current = null; };
    return (
        <section ref={ref} role="region" aria-label={t('pages.deviceAssistant.title')}
            hidden={!open} inert={!open ? true : undefined}
            className="absolute z-30 flex flex-col overflow-hidden rounded-lg border bg-background text-foreground shadow-xl"
            style={{ display: open ? 'flex' : 'none', left: geometry.x, top: geometry.y,
                width: geometry.width, height: geometry.height }}
            onFocusCapture={onFocus}
            onKeyDown={event => event.stopPropagation()} onKeyUp={event => event.stopPropagation()}
            onPointerMove={event => event.stopPropagation()}
            onPointerDown={event => event.stopPropagation()} onPointerUp={event => event.stopPropagation()}
            onMouseDown={event => event.stopPropagation()} onMouseUp={event => event.stopPropagation()}
            onWheel={event => event.stopPropagation()} onTouchStart={event => event.stopPropagation()}
            onTouchMove={event => event.stopPropagation()} onTouchEnd={event => event.stopPropagation()}>
            <header className="flex shrink-0 items-center gap-2 border-b px-3 py-2">
                <div className="flex min-w-0 flex-1 touch-none cursor-move items-center gap-2 select-none"
                    onPointerDown={event => start(event, false)} onPointerMove={move}
                    onPointerUp={stop} onPointerCancel={stop} onLostPointerCapture={stop}>
                    <AiAssistantIcon className="h-5 w-5 shrink-0" />
                    <span className="truncate font-medium">{t('pages.deviceAssistant.title')}</span>
                    <GripHorizontal className="ml-auto h-4 w-4 shrink-0 text-muted-foreground" />
                </div>
                <Button ref={closeRef} type="button" variant="ghost" size="icon" onClick={onClose}
                    aria-label={t('pages.deviceAssistant.hidePanel')}><X className="h-4 w-4" /></Button>
            </header>
            <div className="min-h-0 flex-1 overflow-y-auto overscroll-contain p-3 [overflow-wrap:anywhere]">
                {children}
            </div>
            <div className="flex h-5 shrink-0 justify-end border-t">
                <div className="touch-none cursor-nwse-resize px-1" aria-hidden="true"
                    onPointerDown={event => start(event, true)} onPointerMove={move}
                    onPointerUp={stop} onPointerCancel={stop} onLostPointerCapture={stop}>
                    <Grip className="h-4 w-4 text-muted-foreground" />
                </div>
            </div>
        </section>
    );
}
