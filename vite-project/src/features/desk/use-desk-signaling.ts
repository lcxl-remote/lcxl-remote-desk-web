import { useEffect, useRef, useState, useCallback } from 'react';
import { v4 } from 'uuid';
import { SIGNALING_TYPE_CODE_HEARTBEAT } from './constants';

export type SignalingMessage = {
    request_id?: string;
    signaling_type: number;
    signaling_data: any;
    to_session_id?: string;
};

const HEARTBEAT_INTERVAL_MS = 30_000;
const HEARTBEAT_TIMEOUT_MS = 60_000; // 2 missed heartbeats = dead
const RECONNECT_BASE_DELAY_MS = 1_000;
const RECONNECT_MAX_DELAY_MS = 30_000;

export function useDeskSignaling(deskId: string | null) {
    const socketRef = useRef<WebSocket | null>(null);
    const [isConnected, setIsConnected] = useState(false);
    const [lastMessage, setLastMessage] = useState<SignalingMessage | null>(null);
    const messageQueue = useRef<SignalingMessage[]>([]);

    // Heartbeat state
    const heartbeatTimerRef = useRef<number | null>(null);
    const lastHeartbeatResponseRef = useRef<number>(Date.now());

    // Reconnection state
    const reconnectAttemptRef = useRef<number>(0);
    const reconnectTimerRef = useRef<number | null>(null);
    const intentionalCloseRef = useRef<boolean>(false);

    const clearHeartbeat = useCallback(() => {
        if (heartbeatTimerRef.current !== null) {
            clearInterval(heartbeatTimerRef.current);
            heartbeatTimerRef.current = null;
        }
    }, []);

    const clearReconnectTimer = useCallback(() => {
        if (reconnectTimerRef.current !== null) {
            clearTimeout(reconnectTimerRef.current);
            reconnectTimerRef.current = null;
        }
    }, []);

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
            reconnectAttemptRef.current = 0;

            // Send queued messages
            while (messageQueue.current.length > 0) {
                const msg = messageQueue.current.shift();
                if (msg) {
                    ws.send(JSON.stringify(msg));
                }
            }

            // Start heartbeat
            lastHeartbeatResponseRef.current = Date.now();
            clearHeartbeat();
            heartbeatTimerRef.current = window.setInterval(() => {
                if (ws.readyState !== WebSocket.OPEN) return;

                // Check if heartbeat timed out
                const elapsed = Date.now() - lastHeartbeatResponseRef.current;
                if (elapsed > HEARTBEAT_TIMEOUT_MS) {
                    console.warn(`Heartbeat timeout (${elapsed}ms), closing WebSocket to trigger reconnect`);
                    ws.close();
                    return;
                }

                // Send heartbeat
                const heartbeat: SignalingMessage = {
                    request_id: v4(),
                    signaling_type: SIGNALING_TYPE_CODE_HEARTBEAT,
                    signaling_data: null,
                };
                ws.send(JSON.stringify(heartbeat));
            }, HEARTBEAT_INTERVAL_MS);
        };

        ws.onclose = () => {
            if (socketRef.current !== ws) return;
            console.log('WebSocket disconnected');
            setIsConnected(false);
            socketRef.current = null;
            clearHeartbeat();

            // Schedule reconnect if not intentional close
            if (!intentionalCloseRef.current) {
                const attempt = reconnectAttemptRef.current;
                const delay = Math.min(RECONNECT_BASE_DELAY_MS * Math.pow(2, attempt), RECONNECT_MAX_DELAY_MS);
                console.log(`Scheduling reconnect attempt ${attempt + 1} in ${delay}ms`);
                reconnectAttemptRef.current = attempt + 1;
                clearReconnectTimer();
                reconnectTimerRef.current = window.setTimeout(() => {
                    connect();
                }, delay);
            }
        };

        ws.onerror = (error) => {
            if (socketRef.current !== ws) return;
            console.error('WebSocket error:', error);
        };

        ws.onmessage = (event) => {
            if (socketRef.current !== ws) return;
            try {
                const message = JSON.parse(event.data) as SignalingMessage;

                // Track heartbeat responses
                if (message.signaling_type === SIGNALING_TYPE_CODE_HEARTBEAT) {
                    lastHeartbeatResponseRef.current = Date.now();
                    return; // Don't propagate heartbeat to consumers
                }

                setLastMessage(message);
            } catch (e) {
                console.error('Failed to parse signaling message', e);
            }
        };
    }, [clearHeartbeat, clearReconnectTimer]);

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
        intentionalCloseRef.current = false;
        const timer = setTimeout(() => {
            connect();
        }, 300);
        return () => {
            clearTimeout(timer);
            intentionalCloseRef.current = true;
            clearHeartbeat();
            clearReconnectTimer();
            if (socketRef.current) {
                socketRef.current.close();
                socketRef.current = null;
            }
        };
    }, [connect, clearHeartbeat, clearReconnectTimer]);

    return { isConnected, lastMessage, sendMessage };
}
