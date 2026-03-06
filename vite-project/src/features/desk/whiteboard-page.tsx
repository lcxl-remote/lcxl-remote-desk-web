import { useEffect, useRef, useState, useCallback } from 'react';
import type { WhiteboardElement } from './use-desk-whiteboard';
import { renderElements } from './whiteboard-canvas';

/**
 * Whiteboard page rendered inside the Tauri overlay WebviewWindow.
 * Receives drawing commands from the server via Tauri events.
 * Full-screen transparent canvas, mouse events pass through.
 */
export default function WhiteboardPage() {
    const canvasRef = useRef<HTMLCanvasElement>(null);
    const [elements, setElements] = useState<WhiteboardElement[]>([]);

    // Listen for Tauri whiteboard-draw events
    useEffect(() => {
        let unlisten: (() => void) | null = null;

        const setup = async () => {
            try {
                const { listen } = await import('@tauri-apps/api/event');
                const unlistenFn = await listen('whiteboard-draw', (event: any) => {
                    try {
                        const msg = typeof event.payload === 'string' ? JSON.parse(event.payload) : event.payload;
                        handleMessage(msg);
                    } catch (e) {
                        console.error('Failed to parse whiteboard message:', e);
                    }
                });
                unlisten = unlistenFn;
            } catch (_e) {
                console.warn('Failed to setup Tauri event listener (not in Tauri context?):', _e);
            }
        };

        setup();

        return () => {
            if (unlisten) unlisten();
        };
    }, []);

    const handleMessage = useCallback((msg: any) => {
        switch (msg.type) {
            case 'draw':
                setElements(prev => [...prev, {
                    id: msg.id,
                    type: 'draw',
                    tool: msg.tool,
                    points: msg.points,
                    color: msg.color,
                    width: msg.width,
                }]);
                break;
            case 'text':
                setElements(prev => [...prev, {
                    id: msg.id,
                    type: 'text',
                    x: msg.x,
                    y: msg.y,
                    content: msg.content,
                    color: msg.color,
                    fontSize: msg.fontSize,
                }]);
                break;
            case 'erase':
                setElements(prev => prev.filter(el => !msg.ids.includes(el.id)));
                break;
            case 'clear':
                setElements([]);
                break;
            case 'undo':
                setElements(prev => prev.slice(0, -1));
                break;
        }
    }, []);

    // Re-render when elements change
    useEffect(() => {
        const canvas = canvasRef.current;
        if (!canvas) return;
        const ctx = canvas.getContext('2d');
        if (!ctx) return;

        canvas.width = window.innerWidth;
        canvas.height = window.innerHeight;

        renderElements(ctx, elements, canvas.width, canvas.height);
    }, [elements]);

    // Handle window resize
    useEffect(() => {
        const handleResize = () => {
            const canvas = canvasRef.current;
            if (!canvas) return;
            canvas.width = window.innerWidth;
            canvas.height = window.innerHeight;
            const ctx = canvas.getContext('2d');
            if (ctx) renderElements(ctx, elements, canvas.width, canvas.height);
        };
        window.addEventListener('resize', handleResize);
        return () => window.removeEventListener('resize', handleResize);
    }, [elements]);

    return (
        <div style={{
            position: 'fixed', top: 0, left: 0, right: 0, bottom: 0,
            background: 'transparent', overflow: 'hidden',
            // Cursor events are ignored at the Tauri window level
        }}>
            <canvas
                ref={canvasRef}
                style={{ width: '100%', height: '100%' }}
            />
        </div>
    );
}
