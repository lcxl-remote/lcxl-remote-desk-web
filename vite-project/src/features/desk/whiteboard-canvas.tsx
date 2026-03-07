import { useRef, useEffect, useMemo } from 'react';
import type { WhiteboardElement } from './use-desk-whiteboard';

type WhiteboardCanvasProps = {
    elements: WhiteboardElement[];
    isActive: boolean;
    onPointerDown: (e: React.PointerEvent) => void;
    onPointerMove: (e: React.PointerEvent) => void;
    onPointerUp: () => void;
    onClick?: (e: React.MouseEvent) => void;
};

function renderElements(ctx: CanvasRenderingContext2D, elements: WhiteboardElement[], width: number, height: number) {
    ctx.clearRect(0, 0, width, height);

    for (const el of elements) {
        if (el.type === 'draw' && el.points && el.points.length > 1) {
            ctx.beginPath();
            ctx.strokeStyle = el.color;
            ctx.lineWidth = el.width || 3;
            ctx.lineCap = 'round';
            ctx.lineJoin = 'round';
            const first = el.points[0];
            ctx.moveTo(first.x * width, first.y * height);
            for (let i = 1; i < el.points.length; i++) {
                ctx.lineTo(el.points[i].x * width, el.points[i].y * height);
            }
            ctx.stroke();
        } else if (el.type === 'text' && el.x !== undefined && el.y !== undefined && el.content) {
            ctx.font = `${el.fontSize || 24}px sans-serif`;
            ctx.fillStyle = el.color;
            ctx.fillText(el.content, el.x * width, el.y * height);
        }
    }
}

export default function WhiteboardCanvas({
    elements, isActive,
    onPointerDown, onPointerMove, onPointerUp, onClick,
}: WhiteboardCanvasProps) {
    const canvasRef = useRef<HTMLCanvasElement>(null);

    // Re-render canvas when elements change
    useEffect(() => {
        const canvas = canvasRef.current;
        if (!canvas) return;
        const ctx = canvas.getContext('2d');
        if (!ctx) return;

        // Match canvas internal resolution to its display size
        const rect = canvas.getBoundingClientRect();
        if (canvas.width !== rect.width || canvas.height !== rect.height) {
            canvas.width = rect.width;
            canvas.height = rect.height;
        }

        renderElements(ctx, elements, canvas.width, canvas.height);
    }, [elements]);

    // Observe resize
    useEffect(() => {
        const canvas = canvasRef.current;
        if (!canvas) return;

        const observer = new ResizeObserver(() => {
            const rect = canvas.getBoundingClientRect();
            canvas.width = rect.width;
            canvas.height = rect.height;
            const ctx = canvas.getContext('2d');
            if (ctx) renderElements(ctx, elements, canvas.width, canvas.height);
        });
        observer.observe(canvas);
        return () => observer.disconnect();
    }, [elements]);

    const style = useMemo(() => ({
        position: 'absolute' as const,
        top: 0, left: 0, right: 0, bottom: 0,
        width: '100%', height: '100%',
        pointerEvents: (isActive ? 'auto' : 'none') as any,
        cursor: isActive ? 'crosshair' : 'default',
        zIndex: 10,
    }), [isActive]);

    if (!isActive && elements.length === 0) return null;

    return (
        <canvas
            ref={canvasRef}
            style={style}
            onPointerDown={onPointerDown}
            onPointerMove={onPointerMove}
            onPointerUp={onPointerUp}
            onPointerLeave={onPointerUp}
            onClick={onClick}
        />
    );
}

// Export render function for use in whiteboard-page.tsx
export { renderElements };
