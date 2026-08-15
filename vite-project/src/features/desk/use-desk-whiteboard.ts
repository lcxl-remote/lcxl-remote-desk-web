import { useRef, useCallback, useEffect, useState } from 'react';
import type { RefObject } from 'react';
import { computeVideoContentRect } from './video-content-rect';

export type WhiteboardTool = 'pen' | 'text';

export type WhiteboardElement = {
    id: string;
    type: 'draw' | 'text';
    tool?: WhiteboardTool;
    points?: { x: number; y: number }[];
    x?: number;
    y?: number;
    content?: string;
    color: string;
    width?: number;
    fontSize?: number;
};

type UseWhiteboardProps = {
    videoRef: RefObject<HTMLVideoElement | null>;
    whiteboardChannel: RefObject<RTCDataChannel | null>;
    isConnected: boolean;
    hasTauri: boolean;
};

export function useDeskWhiteboard({ videoRef, whiteboardChannel, isConnected, hasTauri }: UseWhiteboardProps) {
    const [isActive, setIsActive] = useState(false);
    const [tool, setTool] = useState<WhiteboardTool>('pen');
    const [color, setColor] = useState('#ff0000');
    const [strokeWidth, setStrokeWidth] = useState(3);
    const [fontSize, setFontSize] = useState(24);
    // elements now only stores the in-progress stroke to prevent duplicate rendering with the remote video stream
    const [elements, setElements] = useState<WhiteboardElement[]>([]);
    const [textInput, setTextInput] = useState<{ x: number; y: number; clientX: number; clientY: number } | null>(null);
    const isDrawingRef = useRef(false);
    const currentPointsRef = useRef<{ x: number; y: number }[]>([]);
    const currentIdRef = useRef('');

    const generateId = () => `wb_${Date.now()}_${Math.random().toString(36).slice(2, 7)}`;

    // Convert pixel coordinates to normalized (0.0~1.0) relative to the video content
    const normalizePoint = useCallback((clientX: number, clientY: number) => {
        const video = videoRef.current;
        if (!video) return null;
        const rect = video.getBoundingClientRect();
        const content = computeVideoContentRect(rect.width, rect.height, video.videoWidth, video.videoHeight);
        if (!content) return null;

        let x = (clientX - rect.left - content.offsetX) / content.width;
        let y = (clientY - rect.top - content.offsetY) / content.height;
        x = Math.max(0, Math.min(1, x));
        y = Math.max(0, Math.min(1, y));
        return { x, y };
    }, [videoRef]);

    const sendMessage = useCallback((msg: any) => {
        const channel = whiteboardChannel.current;
        if (channel && channel.readyState === 'open') {
            channel.send(JSON.stringify(msg));
        }
    }, [whiteboardChannel]);

    const handlePointerDown = useCallback((e: React.PointerEvent) => {
        if (!isActive || tool !== 'pen') return;
        const point = normalizePoint(e.clientX, e.clientY);
        if (!point) return;
        isDrawingRef.current = true;
        currentIdRef.current = generateId();
        currentPointsRef.current = [point];
    }, [isActive, tool, normalizePoint]);

    const handlePointerMove = useCallback((e: React.PointerEvent) => {
        if (!isDrawingRef.current) return;
        const point = normalizePoint(e.clientX, e.clientY);
        if (!point) return;
        currentPointsRef.current.push(point);
        // Update local canvas preview with in-progress stroke
        // Only render the current stroke locally
        setElements([{
            id: currentIdRef.current,
            type: 'draw',
            tool: 'pen',
            points: [...currentPointsRef.current],
            color,
            width: strokeWidth,
        }]);
    }, [normalizePoint, color, strokeWidth]);

    const handlePointerUp = useCallback(() => {
        if (!isDrawingRef.current) return;
        isDrawingRef.current = false;
        if (currentPointsRef.current.length > 1) {
            const msg = {
                type: 'draw',
                tool: 'pen',
                points: currentPointsRef.current,
                color,
                width: strokeWidth,
                id: currentIdRef.current,
            };
            sendMessage(msg);
        }
        // Clear the local stroke immediately. The remote video stream will show the drawn line instantly.
        setElements([]);
        currentPointsRef.current = [];
    }, [color, strokeWidth, sendMessage]);
    const handleCanvasClick = useCallback((e: React.MouseEvent) => {
        if (!isActive || tool !== 'text') return;
        // Don't open a new input if one is already open
        if (textInput) return;

        e.stopPropagation();

        const point = normalizePoint(e.clientX, e.clientY);
        if (!point) return;

        setTextInput({
            x: point.x,
            y: point.y,
            clientX: e.clientX,
            clientY: e.clientY
        });
    }, [isActive, tool, normalizePoint, textInput]);

    const confirmTextInput = useCallback((text: string) => {
        if (!textInput || !text.trim()) {
            setTextInput(null);
            return;
        }

        const id = generateId();
        // Send the text directly, no need to store it locally
        sendMessage({
            type: 'text', x: textInput.x, y: textInput.y,
            content: text, color, fontSize, id,
        });
        setTextInput(null);
    }, [textInput, color, fontSize, sendMessage]);

    const cancelTextInput = useCallback(() => {
        setTextInput(null);
    }, []);

    const clearAll = useCallback(() => {
        setElements([]);
        sendMessage({ type: 'clear' });
    }, [sendMessage]);

    const undo = useCallback(() => {
        sendMessage({ type: 'undo' });
    }, [sendMessage]);

    const canActivate = isConnected && hasTauri;
    const isInteractive = isActive && canActivate;

    useEffect(() => {
        if (canActivate) return;
        // Keep the user's active-mode intent across a replacement PC, but discard
        // an unfinished gesture that cannot be delivered to the old host overlay.
        isDrawingRef.current = false;
        currentPointsRef.current = [];
        setElements([]);
        setTextInput(null);
    }, [canActivate]);

    const toggleWhiteboard = useCallback(() => {
        setIsActive(prev => {
            if (!prev && !canActivate) return prev;
            const next = !prev;
            if (!next) {
                // Clear local elements and remote elements when turning off whiteboard
                setElements([]);
                setTextInput(null);
                sendMessage({ type: 'clear' });
            }
            return next;
        });
    }, [canActivate, sendMessage]);

    const deactivateWhiteboard = useCallback(() => {
        if (isActive && isConnected) {
            sendMessage({ type: 'clear' });
        }
        isDrawingRef.current = false;
        currentPointsRef.current = [];
        setElements([]);
        setTextInput(null);
        setIsActive(false);
    }, [isActive, isConnected, sendMessage]);

    return {
        isActive,
        tool, setTool,
        color, setColor,
        strokeWidth, setStrokeWidth,
        fontSize, setFontSize,
        elements,
        textInput,
        confirmTextInput,
        cancelTextInput,
        canActivate,
        isInteractive,
        toggleWhiteboard,
        deactivateWhiteboard,
        clearAll,
        undo,
        handlePointerDown,
        handlePointerMove,
        handlePointerUp,
        handleCanvasClick,
    };
}
