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

    // Force transparent background for the overlay window
    useEffect(() => {
        document.documentElement.style.setProperty('background', 'transparent', 'important');
        document.body.style.setProperty('background', 'transparent', 'important');
        const root = document.getElementById('root');
        if (root) root.style.setProperty('background', 'transparent', 'important');

        return () => {
            document.documentElement.style.removeProperty('background');
            document.body.style.removeProperty('background');
            if (root) root.style.removeProperty('background');
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

    // Listen for window event dispatched by evaluate_script from Rust
    useEffect(() => {
        const handleEvent = (event: Event) => {
            try {
                const customEvent = event as CustomEvent;
                const msg = typeof customEvent.detail === 'string' ? JSON.parse(customEvent.detail) : customEvent.detail;
                handleMessage(msg);
            } catch (e) {
                console.error('Failed to parse whiteboard message:', e);
            }
        };

        window.addEventListener('whiteboard-draw', handleEvent);

        return () => {
            window.removeEventListener('whiteboard-draw', handleEvent);
        };
    }, [handleMessage]);

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
