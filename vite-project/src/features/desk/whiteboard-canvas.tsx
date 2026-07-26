import { useRef, useEffect, useMemo } from 'react';
import type { RefObject } from 'react';
import type { WhiteboardElement } from './use-desk-whiteboard';
import { computeVideoContentRectInOverlay } from './video-content-rect';
import type { VideoContentRect } from './video-content-rect';

type WhiteboardCanvasProps = {
    elements: WhiteboardElement[];
    isActive: boolean;
    /**
     * Video the strokes are anchored to. The canvas covers the whole wrapper,
     * but normalized coordinates address the video content rect inside it.
     */
    videoRef: RefObject<HTMLVideoElement | null>;
    onPointerDown: (e: React.PointerEvent) => void;
    onPointerMove: (e: React.PointerEvent) => void;
    onPointerUp: () => void;
    onClick?: (e: React.MouseEvent) => void;
};

/**
 * Draw normalized (0..1) elements into `target`, the sub-rect of the canvas
 * that holds video pixels. Passing the full canvas rect renders full-bleed,
 * which is what the host-side overlay window wants.
 */
function renderElements(
    ctx: CanvasRenderingContext2D,
    elements: WhiteboardElement[],
    width: number,
    height: number,
    target: VideoContentRect = { offsetX: 0, offsetY: 0, width, height },
) {
    ctx.clearRect(0, 0, width, height);

    const toX = (x: number) => target.offsetX + x * target.width;
    const toY = (y: number) => target.offsetY + y * target.height;

    for (const el of elements) {
        if (el.type === 'draw' && el.points && el.points.length > 1) {
            ctx.beginPath();
            ctx.strokeStyle = el.color;
            ctx.lineWidth = el.width || 3;
            ctx.lineCap = 'round';
            ctx.lineJoin = 'round';
            const first = el.points[0];
            ctx.moveTo(toX(first.x), toY(first.y));
            for (let i = 1; i < el.points.length; i++) {
                ctx.lineTo(toX(el.points[i].x), toY(el.points[i].y));
            }
            ctx.stroke();
        } else if (el.type === 'text' && el.x !== undefined && el.y !== undefined && el.content) {
            ctx.font = `${el.fontSize || 24}px sans-serif`;
            ctx.fillStyle = el.color;
            ctx.fillText(el.content, toX(el.x), toY(el.y));
        }
    }
}

/**
 * Where the video pixels land inside the overlay canvas, or undefined while the
 * video has no frame yet -- callers then fall back to the full canvas.
 */
function videoContentRect(canvas: HTMLCanvasElement, video: HTMLVideoElement | null): VideoContentRect | undefined {
    if (!video) return undefined;
    return computeVideoContentRectInOverlay(
        canvas.getBoundingClientRect(),
        video.getBoundingClientRect(),
        video.videoWidth,
        video.videoHeight,
    ) ?? undefined;
}

export default function WhiteboardCanvas({
    elements, isActive, videoRef,
    onPointerDown, onPointerMove, onPointerUp, onClick,
}: WhiteboardCanvasProps) {
    const canvasRef = useRef<HTMLCanvasElement>(null);

    // Redraw on element changes, on layout changes of either the canvas or the
    // video, and when the stream's intrinsic size changes (resolution switch) --
    // all of them move the video content rect the strokes are anchored to.
    useEffect(() => {
        const canvas = canvasRef.current;
        if (!canvas) return;

        const draw = () => {
            const ctx = canvas.getContext('2d');
            if (!ctx) return;

            // Match canvas internal resolution to its display size
            const rect = canvas.getBoundingClientRect();
            const width = Math.round(rect.width);
            const height = Math.round(rect.height);
            if (canvas.width !== width || canvas.height !== height) {
                canvas.width = width;
                canvas.height = height;
            }

            renderElements(ctx, elements, canvas.width, canvas.height, videoContentRect(canvas, videoRef.current));
        };

        draw();

        const observer = new ResizeObserver(draw);
        observer.observe(canvas);
        const video = videoRef.current;
        if (video) {
            observer.observe(video);
            // `resize` fires when videoWidth/videoHeight change, which
            // `ResizeObserver` alone does not report.
            video.addEventListener('resize', draw);
            video.addEventListener('loadedmetadata', draw);
        }
        return () => {
            observer.disconnect();
            if (video) {
                video.removeEventListener('resize', draw);
                video.removeEventListener('loadedmetadata', draw);
            }
        };
    }, [elements, videoRef]);

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
