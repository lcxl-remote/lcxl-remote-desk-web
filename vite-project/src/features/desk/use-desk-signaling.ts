import { useEffect, useRef, useState, useCallback } from 'react';
import { v4 } from 'uuid';

export type SignalingMessage = {
    request_id?: string;
    signaling_type: number;
    signaling_data: any;
    to_session_id?: string;
};

export function useDeskSignaling(deskId: string | null) {
    const socketRef = useRef<WebSocket | null>(null);
    const [isConnected, setIsConnected] = useState(false);
    const [lastMessage, setLastMessage] = useState<SignalingMessage | null>(null);
    const messageQueue = useRef<SignalingMessage[]>([]);

    const connect = useCallback(() => {
        if (socketRef.current?.readyState === WebSocket.OPEN || socketRef.current?.readyState === WebSocket.CONNECTING) return;

        const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
        const host = window.location.host;
        const url = new URL(`${protocol}//${host}/api/desk/signaling`);

        url.searchParams.append("api_version", "1");
        url.searchParams.append("build_number", "1");
        url.searchParams.append("commit_hash", "1");
        url.searchParams.append("operation_system", "wasm");
        url.searchParams.append("remote_desk_type", "browser");

        const ws = new WebSocket(url.toString());
        socketRef.current = ws;

        ws.onopen = () => {
            if (socketRef.current !== ws) return;
            console.log('WebSocket connected');
            setIsConnected(true);

            // Send queued messages
            while (messageQueue.current.length > 0) {
                const msg = messageQueue.current.shift();
                if (msg) {
                    ws.send(JSON.stringify(msg));
                }
            }


        };

        ws.onclose = () => {
            if (socketRef.current !== ws) return;
            console.log('WebSocket disconnected');
            setIsConnected(false);
            socketRef.current = null;
        };

        ws.onerror = (error) => {
            if (socketRef.current !== ws) return;
            console.error('WebSocket error:', error);
        };

        ws.onmessage = (event) => {
            if (socketRef.current !== ws) return;
            try {
                const message = JSON.parse(event.data) as SignalingMessage;
                setLastMessage(message);
            } catch (e) {
                console.error('Failed to parse signaling message', e);
            }
        };
    }, []);

    const sendMessage = useCallback((type: number, data: any, toSessionId?: string) => {
        const msg: SignalingMessage = {
            request_id: v4(),
            signaling_type: type,
            signaling_data: data,
            to_session_id: toSessionId,
        };

        if (socketRef.current?.readyState === WebSocket.OPEN) {
            socketRef.current.send(JSON.stringify(msg));
        } else {
            console.warn('WebSocket not connected, queuing message', type);
            messageQueue.current.push(msg);
            if (!socketRef.current || socketRef.current.readyState === WebSocket.CLOSED) {
                connect();
            }
        }
    }, [connect]);

    useEffect(() => {
        const timer = setTimeout(() => {
            connect();
        }, 300);
        return () => {
            clearTimeout(timer);
            if (socketRef.current) {
                socketRef.current.close();
                socketRef.current = null;
            }
        };
    }, [connect]);

    return { isConnected, lastMessage, sendMessage };
}
