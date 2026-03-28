import { useEffect, useRef, useState, useCallback } from 'react';
import { v4 } from 'uuid';
import { SIGNALING_TYPE_CODE_HEARTBEAT } from './constants';

export type SignalingMessage = {
    request_id?: string;
    signaling_type: number;
    signaling_data: any;
    to_connection_id?: string;
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
    const heartbeatCheckTimerRef = useRef<number | null>(null);
    const lastHeartbeatResponseRef = useRef<number>(Date.now());

    // Reconnect state
    const reconnectTimerRef = useRef<number | null>(null);
    const reconnectAttemptsRef = useRef<number>(0);
    const intentionalCloseRef = useRef<boolean>(false);

    const clearHeartbeat = useCallback(() => {
        if (heartbeatTimerRef.current !== null) {
            window.clearInterval(heartbeatTimerRef.current);
            heartbeatTimerRef.current = null;
        }
        if (heartbeatCheckTimerRef.current !== null) {
            window.clearInterval(heartbeatCheckTimerRef.current);
            heartbeatCheckTimerRef.current = null;
        }
    }, []);

    const clearReconnectTimer = useCallback(() => {
        if (reconnectTimerRef.current !== null) {
            window.clearTimeout(reconnectTimerRef.current);
            reconnectTimerRef.current = null;
        }
    }, []);

    const connect = useCallback(() => {
        if (intentionalCloseRef.current) return;
        if (socketRef.current && (socketRef.current.readyState === WebSocket.CONNECTING || socketRef.current.readyState === WebSocket.OPEN)) return;

        console.log('Connecting to signaling server...');
        clearReconnectTimer();

        const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
        const host = window.location.host;
        const ws = new WebSocket(`${protocol}//${host}/api/desk/signaling`);
        socketRef.current = ws;

        ws.onopen = () => {
            console.log('Signaling WebSocket connected');
            setIsConnected(true);
            reconnectAttemptsRef.current = 0;
            lastHeartbeatResponseRef.current = Date.now();

            // Setup heartbeat
            heartbeatTimerRef.current = window.setInterval(() => {
                if (ws.readyState === WebSocket.OPEN) {
                    ws.send(JSON.stringify({
                        request_id: v4(),
                        signaling_type: SIGNALING_TYPE_CODE_HEARTBEAT,
                        signaling_data: null,
                    }));
                }
            }, HEARTBEAT_INTERVAL_MS);

            // Heartbeat watchdog
            heartbeatCheckTimerRef.current = window.setInterval(() => {
                const now = Date.now();
                if (now - lastHeartbeatResponseRef.current > HEARTBEAT_TIMEOUT_MS) {
                    console.warn('Signaling heartbeat timed out, reconnecting...');
                    ws.close();
                }
            }, HEARTBEAT_INTERVAL_MS);

            // Process queued messages
            const queue = [...messageQueue.current];
            messageQueue.current = [];
            queue.forEach(msg => {
                ws.send(JSON.stringify(msg));
            });
        };

        ws.onclose = () => {
            console.log('Signaling WebSocket closed');
            setIsConnected(false);
            clearHeartbeat();

            if (!intentionalCloseRef.current) {
                const delay = Math.min(
                    RECONNECT_BASE_DELAY_MS * Math.pow(1.5, reconnectAttemptsRef.current),
                    RECONNECT_MAX_DELAY_MS
                );
                reconnectAttemptsRef.current++;
                console.log(`Reconnecting in ${delay}ms...`);
                reconnectTimerRef.current = window.setTimeout(connect, delay);
            }
        };

        ws.onerror = (error) => {
            console.error('Signaling WebSocket error', error);
        };

        ws.onmessage = (event) => {
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

    const sendMessage = useCallback((type: number, data: any, toConnectionId?: string) => {
        const msg: SignalingMessage = {
            request_id: v4(),
            signaling_type: type,
            signaling_data: data,
            to_connection_id: toConnectionId,
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
        connect();

        return () => {
            console.log('Cleaning up signaling connection');
            intentionalCloseRef.current = true;
            clearHeartbeat();
            clearReconnectTimer();
            if (socketRef.current) {
                socketRef.current.onclose = null;
                socketRef.current.close();
                socketRef.current = null;
            }
        };
    }, [connect, clearHeartbeat, clearReconnectTimer]);

    return {
        isConnected,
        lastMessage,
        sendMessage,
    };
}
