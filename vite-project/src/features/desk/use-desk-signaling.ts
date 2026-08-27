import { useEffect, useRef, useState, useCallback } from 'react';
import { v4 } from 'uuid';
import {
    SIGNALING_API_VERSION,
    SIGNALING_TYPE_CODE_HEARTBEAT_ACKNOWLEDGED,
    SIGNALING_TYPE_CODE_SEND_HEARTBEAT,
} from './constants';

export type SignalingMessage = {
    request_id?: string;
    signaling_type: number;
    signaling_data: any;
    to_connection_id?: string;
    response_state?: {
        error_code: number;
        message?: string | null;
    } | null;
};

/** A handler invoked synchronously for every inbound non-heartbeat
 *  signaling message, in arrival order. Registered via `subscribe`. */
export type SignalingSubscriber = (msg: SignalingMessage) => void;

/**
 * Options for {@link useDeskSignaling}'s `sendTracked`. `replaceKey`
 * collapses superseded messages still waiting in the offline queue (only
 * the newest with a given key survives); `onSent` fires exactly once when
 * the message genuinely reaches the wire (`ws.send` succeeds), whether
 * that happens immediately or later when a reconnect flushes the queue.
 */
export type SendTrackedOptions = {
    type: number;
    data: any;
    toConnectionId?: string;
    requestId?: string;
    replaceKey?: string;
    /** Logical PeerConnection scope used to purge stale queued callbacks. */
    scope?: string;
    onSent?: (requestId: string) => void;
};

/** Result of `sendTracked`: the wire `request_id` plus whether the
 *  message was sent immediately (`sent`) or parked in the offline queue
 *  (`queued`, to be flushed on the next reconnect). */
export type SendTrackedResult = {
    requestId: string;
    disposition: 'sent' | 'queued';
};

/** An entry in the offline send queue. Carries the optional `replaceKey`
 *  (dedup) and `onSent` (delivery notification) so both survive until the
 *  message is actually flushed. */
type QueuedMessage = {
    msg: SignalingMessage;
    replaceKey?: string;
    scope?: string;
    onSent?: (requestId: string) => void;
};

const HEARTBEAT_INTERVAL_MS = 30_000;
const HEARTBEAT_TIMEOUT_MS = 60_000; // 2 missed heartbeats = dead
const RECONNECT_BASE_DELAY_MS = 1_000;
const RECONNECT_MAX_DELAY_MS = 30_000;

export function useDeskSignaling() {
    const socketRef = useRef<WebSocket | null>(null);
    const [isConnected, setIsConnected] = useState(false);
    const messageQueue = useRef<QueuedMessage[]>([]);
    const subscribersRef = useRef<Set<SignalingSubscriber>>(new Set());

    /**
     * Register a handler invoked synchronously for every inbound
     * non-heartbeat signaling message, in arrival order; returns an
     * unsubscribe function. This is the sole, lossless delivery path: a
     * burst of messages arriving within one tick (e.g. trickled ICE
     * candidates) is delivered in full. Routing the stream through a
     * single React state value instead would let React coalesce rapid
     * updates and silently drop the middle of a burst — the LAN
     * connection-failure root cause this design replaces.
     */
    const subscribe = useCallback((handler: SignalingSubscriber) => {
        subscribersRef.current.add(handler);
        return () => {
            subscribersRef.current.delete(handler);
        };
    }, []);

    /**
     * Append a message to the offline queue. When the item carries a
     * `replaceKey`, any existing queued item with the same key is
     * overwritten in place (so a superseded OFFER does not pile up and
     * the dropped item's `onSent` is never invoked).
     */
    const enqueue = useCallback((item: QueuedMessage) => {
        if (item.replaceKey !== undefined) {
            const idx = messageQueue.current.findIndex(
                (q) => q.replaceKey === item.replaceKey,
            );
            if (idx >= 0) {
                messageQueue.current[idx] = item;
                return;
            }
        }
        messageQueue.current.push(item);
    }, []);

    // Heartbeat state
    const heartbeatTimerRef = useRef<number | null>(null);
    const heartbeatCheckTimerRef = useRef<number | null>(null);
    const lastHeartbeatResponseRef = useRef<number>(Date.now());
    const pendingHeartbeatIdsRef = useRef<Set<string>>(new Set());

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
        pendingHeartbeatIdsRef.current.clear();
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
        const url = new URL(`${protocol}//${host}/api/desk/signaling`);
        url.searchParams.set('api_version', String(SIGNALING_API_VERSION));
        url.searchParams.set('build_number', '0');
        url.searchParams.set('commit_hash', 'web');
        url.searchParams.set('operation_system', 'Web');
        url.searchParams.set('remote_desk_type', 'browser');
        const ws = new WebSocket(url.toString());
        socketRef.current = ws;

        ws.onopen = () => {
            console.log('Signaling WebSocket connected');
            setIsConnected(true);
            reconnectAttemptsRef.current = 0;
            lastHeartbeatResponseRef.current = Date.now();

            // Setup heartbeat
            heartbeatTimerRef.current = window.setInterval(() => {
                if (ws.readyState === WebSocket.OPEN) {
                    const requestId = v4();
                    pendingHeartbeatIdsRef.current.add(requestId);
                    ws.send(JSON.stringify({
                        request_id: requestId,
                        signaling_type: SIGNALING_TYPE_CODE_SEND_HEARTBEAT,
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

            // Process queued messages. Each successful `ws.send` fires the
            // item's `onSent` (delivery notification); a send that throws
            // re-queues the item for the next reconnect and does NOT fire
            // `onSent`, so callers never treat an undelivered message as
            // sent.
            const queue = [...messageQueue.current];
            messageQueue.current = [];
            queue.forEach(item => {
                try {
                    ws.send(JSON.stringify(item.msg));
                    item.onSent?.(item.msg.request_id!);
                } catch (e) {
                    console.warn('Failed to flush queued signaling message, re-queuing', e);
                    messageQueue.current.push(item);
                }
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
                if (message.signaling_type === SIGNALING_TYPE_CODE_HEARTBEAT_ACKNOWLEDGED) {
                    const requestId = message.request_id;
                    if (requestId && pendingHeartbeatIdsRef.current.delete(requestId)) {
                        lastHeartbeatResponseRef.current = Date.now();
                    }
                    return; // Don't propagate heartbeat to consumers
                }

                // Hand every message to each subscriber synchronously, in
                // arrival order. A bursty stream routed through a single
                // React state value would be coalesced by rendering down to
                // its first and last value, silently dropping everything
                // between; synchronous fan-out here cannot.
                subscribersRef.current.forEach((handler) => {
                    try {
                        handler(message);
                    } catch (e) {
                        console.error('Signaling subscriber threw', e);
                    }
                });
            } catch (e) {
                console.error('Failed to parse signaling message', e);
            }
        };
    }, [clearHeartbeat, clearReconnectTimer]);

    /**
     * Send a signaling message. Returns the actual `request_id` that
     * went on the wire so callers can correlate the eventual response.
     *
     * `requestId` is optional — when omitted a fresh UUID is generated
     * (the historical behaviour). Pass an explicit id when the caller
     * needs to recognise its own echo (e.g. the adaptive-resolution
     * hook silently drops auto responses by matching on a pending-id
     * set).
     *
     * Earlier call sites that ignored the void return type still
     * compile unchanged.
     */
    const sendMessage = useCallback((
        type: number,
        data: any,
        toConnectionId?: string,
        requestId?: string,
    ): string => {
        const id = requestId ?? v4();
        const msg: SignalingMessage = {
            request_id: id,
            signaling_type: type,
            signaling_data: data,
            to_connection_id: toConnectionId,
        };

        if (socketRef.current?.readyState === WebSocket.OPEN) {
            socketRef.current.send(JSON.stringify(msg));
        } else {
            console.warn('WebSocket not connected, queuing message', type);
            enqueue({ msg });
            if (!socketRef.current || socketRef.current.readyState === WebSocket.CLOSED) {
                connect();
            }
        }
        return id;
    }, [connect, enqueue]);

    /**
     * Like {@link sendMessage} but reports delivery: returns
     * `{ requestId, disposition }` and (via `opts.onSent`) notifies the
     * caller when the message actually reaches the wire. Used by the RTC
     * retry coordinator so an OFFER's ANSWER watchdog only starts once the
     * OFFER is genuinely sent — never while it sits queued offline. A
     * `ws.send` that throws parks the message in the queue (disposition
     * `queued`) instead of dropping it.
     */
    const sendTracked = useCallback((opts: SendTrackedOptions): SendTrackedResult => {
        const id = opts.requestId ?? v4();
        const msg: SignalingMessage = {
            request_id: id,
            signaling_type: opts.type,
            signaling_data: opts.data,
            to_connection_id: opts.toConnectionId,
        };

        if (socketRef.current?.readyState === WebSocket.OPEN) {
            try {
                socketRef.current.send(JSON.stringify(msg));
                opts.onSent?.(id);
                return { requestId: id, disposition: 'sent' };
            } catch (e) {
                console.warn('sendTracked: ws.send failed, queuing message', opts.type, e);
                enqueue({ msg, replaceKey: opts.replaceKey, scope: opts.scope, onSent: opts.onSent });
                return { requestId: id, disposition: 'queued' };
            }
        }

        enqueue({ msg, replaceKey: opts.replaceKey, scope: opts.scope, onSent: opts.onSent });
        if (!socketRef.current || socketRef.current.readyState === WebSocket.CLOSED) {
            connect();
        }
        return { requestId: id, disposition: 'queued' };
    }, [connect, enqueue]);

    /**
     * Drop every still-queued message carrying the given `replaceKey`
     * without sending it (its `onSent` is never invoked). Lets the RTC
     * coordinator purge a pending OFFER on teardown so a later reconnect
     * doesn't replay a stale negotiation.
     */
    const cancelQueued = useCallback((replaceKey: string) => {
        messageQueue.current = messageQueue.current.filter(
            (q) => q.replaceKey !== replaceKey,
        );
    }, []);

    const cancelQueuedScope = useCallback((scope: string) => {
        messageQueue.current = messageQueue.current.filter(
            (q) => q.scope !== scope,
        );
    }, []);

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
        subscribe,
        sendMessage,
        sendTracked,
        cancelQueued,
        cancelQueuedScope,
    };
}
