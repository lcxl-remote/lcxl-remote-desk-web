import { useRef, useState, useCallback } from 'react';
import { v4 as uuidv4 } from 'uuid';
import {
    SIGNALING_API_VERSION,
    SIGNALING_TYPE_CODE_REQUEST_REMOTE_ACCESS,
    SIGNALING_TYPE_CODE_REMOTE_ACCESS_INITIALIZED,
    SIGNALING_TYPE_CODE_OFFER,
    SIGNALING_TYPE_CODE_ANSWER,
    SIGNALING_TYPE_CODE_ICE_CANDIDATE,
    SIGNALING_TYPE_CODE_GET_SYSTEM_INFO,
    SIGNALING_TYPE_CODE_SYSTEM_INFO_RETRIEVED,
    SIGNALING_TYPE_CODE_LIST_FILES,
    SIGNALING_TYPE_CODE_FILES_LISTED,
    SIGNALING_TYPE_CODE_DELETE_FILE,
    SIGNALING_TYPE_CODE_FILE_DELETED,
    SIGNALING_TYPE_CODE_CLOSE_REMOTE_SESSION,
    SIGNALING_TYPE_CODE_ERROR,
} from '../desk/constants';
import { createAcceptGate } from './upload-accept-gate';
import { readSessionGrant } from '@/features/desk/session-grant';
import {
    BufferedDownloadSink,
    StreamingDownloadSink,
    type DownloadSink,
    type WritableFileStreamLike,
} from './download-sink';
import {
    buildBinaryChunk,
    FILE_TRANSFER_CHUNK_SIZE,
    parseBinaryChunk,
    type DownloadRequest,
    type DownloadResponse,
    type FileTransferMessage,
    type TransferComplete,
    type TransferError,
    type UploadRequest,
    type UploadResponse,
} from './file-transfer-protocol';
import {
    canStreamToDisk,
    fallbackBlobSaver,
    openStreamingWritable,
} from './file-save';
import { TransferRegistry } from './transfer-registry';
import {
    createDiagnosticsCollector,
    type ConnectionDiagnostics,
    type DiagnosticsCollector,
} from './connection-diagnostics';
import { deskErrorCodeEnum, type SystemInfo } from '@/services/types';

/**
 * A signaling request the host answered with an error frame.
 *
 * The host sends a numeric `DeskErrorCode` plus raw English text; rejecting with
 * a plain `Error` would keep only the text, forcing callers to display the
 * backend's English or match on it. Carrying the code lets them localize.
 *
 * Still an `Error` subclass, so existing `instanceof Error` / `error.message`
 * handling is unaffected.
 */
export class SignalingError extends Error {
    /** `DeskErrorCode` from the response frame. */
    readonly code: number;

    constructor(message: string, code: number) {
        super(message);
        this.name = 'SignalingError';
        this.code = code;
    }
}

/**
 * Which stage of the connection failed locally, as opposed to being refused by
 * the host.
 *
 * The two stages fail for different reasons and the user can act on different
 * things: a session that never comes up means the central or the host is
 * unreachable, while a data channel that never opens means media/relay
 * connectivity is broken but browsing still works. Carrying the stage lets the
 * page say which, instead of showing one generic timeout for both.
 */
export type ConnectionFailureKind =
    | 'session-timeout'
    | 'session-closed'
    | 'channel-timeout'
    | 'ice-failed'
    | 'channel-closed';

/**
 * A locally-detected connection failure (timeout, socket loss, ICE failure).
 *
 * It carries `TIMEOUT` as its `DeskErrorCode` so callers that only read codes
 * keep working, while `kind` is what a caller uses to name the actual stage.
 */
export class ConnectionError extends Error {
    readonly kind: ConnectionFailureKind;
    readonly code: number;

    constructor(kind: ConnectionFailureKind, message: string) {
        super(message);
        this.name = 'ConnectionError';
        this.kind = kind;
        this.code = deskErrorCodeEnum.TIMEOUT;
    }
}

export function isConnectionError(error: unknown): error is ConnectionError {
    return error instanceof ConnectionError;
}

/**
 * How long the signaling session (WebSocket + `RequestRemoteAccess` round trip)
 * may take.
 *
 * This is a single signaling round trip, so a healthy central answers in well
 * under a second. Timing it separately — rather than sharing one budget with
 * WebRTC — is what lets an unreachable central be reported promptly instead of
 * after the entire ICE allowance has been spent.
 */
export const SESSION_TIMEOUT_MS = 10_000;

/**
 * How long the data channel may take once the session is up. ICE can legitimately
 * be slow on a poor network, so this keeps the original budget; it simply no
 * longer has the session handshake subtracted from it.
 */
export const DATA_CHANNEL_TIMEOUT_MS = 20_000;

/**
 * Bounded retries for a host that answers `RequestRemoteAccess` with
 * `ACTION_NEED_RETRY` because it is still waiting for its manager credential
 * proof. The desk and terminal sessions retry the same way.
 */
const REMOTE_ACCESS_RETRY_LIMIT = 3;
const REMOTE_ACCESS_RETRY_DELAY_MS = 500;

/** The host's answer to `RequestRemoteAccess`. Fields are read defensively: it is
 * whatever the host and the central put on the wire. */
interface RemoteAccessInit {
    ice_servers?: unknown;
    connection_epoch?: unknown;
}

/** A live signaling session: the socket plus the host's initialization payload. */
interface SignalingSession {
    ws: WebSocket;
    init: RemoteAccessInit;
}

/** An in-flight connection attempt, shared by every caller that awaits it. */
interface Attempt<T> {
    promise: Promise<T>;
    resolve: (value: T) => void;
    reject: (error: Error) => void;
    timeout?: ReturnType<typeof setTimeout>;
}

function createAttempt<T>(): Attempt<T> {
    let resolve!: (value: T) => void;
    let reject!: (error: Error) => void;
    const promise = new Promise<T>((res, rej) => {
        resolve = res;
        reject = rej;
    });
    return { promise, resolve, reject };
}

/**
 * Unbind and close a socket.
 *
 * Unbinding first is what makes this safe to call while replacing a connection:
 * the discarded socket's `onclose` must not run against state that now belongs
 * to its successor.
 */
function detachAndCloseSocket(socket: WebSocket | null) {
    if (!socket) return;
    socket.onopen = null;
    socket.onmessage = null;
    socket.onerror = null;
    socket.onclose = null;
    try {
        socket.close();
    } catch {
        // Already closing or closed — nothing to release.
    }
}

// --- Transfer state ---

export interface TransferProgress {
    transferId: string;
    fileName: string;
    fileSize: number;
    direction: 'download' | 'upload';
    status: 'connecting' | 'transferring' | 'completed' | 'error';
    progress: number; // 0-100
    transferredBytes: number;
    speed: number; // bytes per second
    remainingSeconds: number; // estimated remaining seconds
    errorMessage?: string;
    /**
     * `DeskErrorCode` for a failed transfer, from the host's `transfer_error`
     * or minted locally for a client-side failure. The view localizes from
     * this and falls back to `errorMessage` for codes it has no text for.
     */
    errorCode?: number;
}

/** Where the file-transfer data channel currently stands. */
export type TransferChannelStatus = 'idle' | 'connecting' | 'ready' | 'failed';

/** Why the data channel is unavailable, with the evidence behind it. */
export interface TransferChannelFailure {
    /** Set when the failure was detected locally. */
    kind: ConnectionFailureKind | null;
    /** Set when the host or central refused the request. */
    errorCode?: number;
    /** The raw text that came with the failure, if any. */
    message: string | null;
    /** What was observed while trying to connect, for display and for a report. */
    diagnostics: ConnectionDiagnostics;
}

// Lightweight per-download progress metadata. The actual file bytes go
// straight into the DownloadSink (streamed to disk or, on the fallback
// path, buffered inside `BufferedDownloadSink`) — they are deliberately
// NOT held here so peak memory does not track the file size.
interface DownloadMeta {
    fileName: string;
    fileSize: number;
    totalChunks: number;
    receivedChunks: number;
    transferredBytes: number;
}


export function useFileTransfer(deskId: string | undefined, orgId?: number) {
    const wsRef = useRef<WebSocket | null>(null);
    const pcRef = useRef<RTCPeerConnection | null>(null);
    const dcRef = useRef<RTCDataChannel | null>(null);
    const [transfers, setTransfers] = useState<TransferProgress[]>([]);
    const downloadMetas = useRef<Map<string, DownloadMeta>>(new Map());
    // Per-transfer download landing strategy (streaming-to-disk or
    // buffered fallback), keyed by transfer_id so interleaved chunks of
    // concurrent downloads route to the correct sink.
    const downloadSinks = useRef<Map<string, DownloadSink>>(new Map());
    // The transfers this tab started and has not yet settled. Inbound messages
    // are matched against it, so a reply for an id we already gave up on
    // cannot resurrect the row, and a transfer that stops receiving data is
    // ended by its watchdog rather than sitting on the screen forever.
    const activeTransfers = useRef(new TransferRegistry());
    // Gate that holds an upload's chunk loop until the host accepts.
    const acceptGate = useRef(createAcceptGate());
    // Track transfer speed state for EMA smoothing
    const transferSpeedState = useRef<Map<string, {
        startTime: number;
        lastCalcTime: number;
        lastCalcBytes: number;
        lastUIUpdate: number;
        emaSpeed: number;
    }>>(new Map());

    // The connection is two planes with separate lifetimes. The signaling
    // session (WebSocket + `RequestRemoteAccess`) is all that directory listing,
    // deletion and host queries need — the host admits the connection when it
    // answers that request, long before any peer connection exists. Only file
    // bytes need the data channel on top of it. Keeping the two apart is what
    // lets browsing survive a WebRTC path that cannot be established at all.
    const sessionRef = useRef<SignalingSession | null>(null);
    const sessionAttemptRef = useRef<Attempt<SignalingSession> | null>(null);
    const channelAttemptRef = useRef<Attempt<RTCDataChannel> | null>(null);
    const remoteAccessRetryRef = useRef(0);
    const retryTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
    const diagnosticsRef = useRef<DiagnosticsCollector>(createDiagnosticsCollector());
    const [channelStatus, setChannelStatus] = useState<TransferChannelStatus>('idle');
    const [channelFailure, setChannelFailure] = useState<TransferChannelFailure | null>(null);

    const pendingRequests = useRef(new Map<string, {
        resolve: (value: any) => void;
        reject: (error: Error) => void;
        timeout: ReturnType<typeof setTimeout>;
        expectedResponseType: number;
    }>());

    // Update a transfer in the list (simple merge, no computation)
    const updateTransfer = useCallback((transferId: string, updates: Partial<TransferProgress>) => {
        setTransfers(prev =>
            prev.map(t => t.transferId === transferId ? { ...t, ...updates } : t)
        );
    }, []);

    // Compute speed and remaining time synchronously (outside React state callback)
    const computeSpeedInfo = useCallback((transferId: string, transferredBytes: number, fileSize: number): { speed: number; remainingSeconds: number } => {
        const state = transferSpeedState.current.get(transferId);
        if (!state || fileSize <= 0) return { speed: 0, remainingSeconds: 0 };

        const now = Date.now();
        const dt = (now - state.lastCalcTime) / 1000;

        if (dt >= 0.1 && transferredBytes > state.lastCalcBytes) {
            const bytesDelta = transferredBytes - state.lastCalcBytes;
            const instantSpeed = bytesDelta / dt;
            // EMA: α=0.3
            state.emaSpeed = state.emaSpeed > 0
                ? 0.3 * instantSpeed + 0.7 * state.emaSpeed
                : instantSpeed;
            state.lastCalcTime = now;
            state.lastCalcBytes = transferredBytes;
        }

        const remaining = fileSize - transferredBytes;
        const remainingSeconds = state.emaSpeed > 0 ? remaining / state.emaSpeed : 0;
        return { speed: state.emaSpeed, remainingSeconds };
    }, []);


    // Remove a completed/errored transfer after delay
    const removeTransferAfterDelay = useCallback((transferId: string, delayMs = 60000) => {
        setTimeout(() => {
            setTransfers(prev => prev.filter(t => t.transferId !== transferId));
        }, delayMs);
    }, []);

    // Best-effort send of a control message to the host. Swallows any
    // error: this runs from failure-cleanup paths where a second
    // failure must not produce an unhandled rejection.
    const sendControlBestEffort = useCallback((msg: FileTransferMessage) => {
        try {
            if (dcRef.current && dcRef.current.readyState === 'open') {
                dcRef.current.send(JSON.stringify(msg));
            }
        } catch {
            // ignore — the channel is already gone
        }
    }, []);

    // Tear down a download's local state. Every step is best-effort so
    // a failure while handling a failure cannot escape as an unhandled
    // rejection (e.g. `writable.abort()` itself rejecting).
    const cleanupDownload = useCallback(async (transferId: string) => {
        const sink = downloadSinks.current.get(transferId);
        downloadSinks.current.delete(transferId);
        downloadMetas.current.delete(transferId);
        transferSpeedState.current.delete(transferId);
        if (sink) {
            try {
                await sink.abort();
            } catch {
                // ignore — releasing the handle is best-effort
            }
        }
    }, []);

    /**
     * The one way a transfer ends.
     *
     * Nine paths reach here — host completion, host error, the inactivity
     * watchdog, a local write failure, cancel, manual remove, disconnect,
     * unmount, and a failed start — and each of them used to clean up a
     * different subset: the cancelled-id set kept download ids forever,
     * completion left the speed state behind, and closing the connection
     * abandoned open sinks. Routing them all through one idempotent call is
     * what makes the watchdog safe to add on top.
     *
     * Returns whether this call is the one that ended the transfer; a second
     * caller for the same id gets `false` and must not touch the row again.
     */
    const settleTransfer = useCallback((
        transferId: string,
        outcome?: { status: 'completed' | 'error'; errorMessage?: string; errorCode?: number },
    ): boolean => {
        if (!activeTransfers.current.settle(transferId)) return false;
        // Reject rather than clear: an upload still parked on the gate has to
        // be woken, or its `await` never returns and the row stays
        // "connecting" for the life of the tab.
        acceptGate.current.reject(transferId, outcome?.errorMessage ?? 'Transfer ended');
        void cleanupDownload(transferId);
        if (outcome) {
            updateTransfer(transferId, {
                status: outcome.status,
                errorMessage: outcome.errorMessage,
                errorCode: outcome.errorCode,
                ...(outcome.status === 'completed' ? { progress: 100 } : {}),
            });
            removeTransferAfterDelay(transferId);
        }
        return true;
    }, [cleanupDownload, updateTransfer, removeTransferAfterDelay]);

    // Handle a download write/finalize failure: mark error, release the
    // sink, and ask the host to stop sending. Best-effort throughout.
    const failDownload = useCallback((transferId: string, message: string, errorCode?: number) => {
        if (!settleTransfer(transferId, { status: 'error', errorMessage: message, errorCode })) {
            return;
        }
        sendControlBestEffort({ type: 'transfer_cancel', transfer_id: transferId });
    }, [settleTransfer, sendControlBestEffort]);

    const handleControlMessage = useCallback(async (msg: FileTransferMessage) => {
        // Every inbound message names a transfer. Only ones this tab started
        // and has not settled are acted on: a reply for anything else would
        // build state for a transfer nobody is waiting on, which is how a
        // timed-out row used to come back to life as `transferring`.
        if (!activeTransfers.current.touch(msg.transfer_id)) return;
        switch (msg.type) {
            case 'download_response': {
                const resp = msg as DownloadResponse;
                downloadMetas.current.set(resp.transfer_id, {
                    fileName: resp.file_name,
                    fileSize: resp.file_size,
                    totalChunks: resp.total_chunks,
                    receivedChunks: 0,
                    transferredBytes: 0,
                });
                // Streaming sinks are created up-front in `downloadFile`
                // (inside the user gesture). The buffered fallback sink
                // needs `total_chunks`, so it is created here.
                if (!downloadSinks.current.has(resp.transfer_id)) {
                    downloadSinks.current.set(
                        resp.transfer_id,
                        new BufferedDownloadSink(resp.total_chunks, resp.file_name, fallbackBlobSaver),
                    );
                }
                updateTransfer(resp.transfer_id, { status: 'transferring', fileSize: resp.file_size });
                // Reset speed state to when actual data transfer begins
                transferSpeedState.current.set(resp.transfer_id, { startTime: Date.now(), lastCalcTime: Date.now(), lastCalcBytes: 0, lastUIUpdate: Date.now(), emaSpeed: 0 });
                break;
            }
            case 'upload_response': {
                const resp = msg as UploadResponse;
                if (resp.accepted) {
                    // Release the upload's chunk loop.
                    acceptGate.current.accept(resp.transfer_id);
                    updateTransfer(resp.transfer_id, { status: 'transferring' });
                } else {
                    // The host normally refuses via `transfer_error`, not
                    // `accepted:false`; this branch is kept for protocol
                    // completeness.
                    settleTransfer(resp.transfer_id, {
                        status: 'error',
                        errorMessage: resp.message || 'Upload rejected',
                    });
                }
                break;
            }
            case 'transfer_complete': {
                const complete = msg as TransferComplete;
                const sink = downloadSinks.current.get(complete.transfer_id);
                if (sink) {
                    // Download complete — flush all queued writes then
                    // close. A finalize failure (disk full, revoked
                    // permission) surfaces as a transfer error.
                    try {
                        await sink.finalize();
                    } catch (e) {
                        failDownload(complete.transfer_id, e instanceof Error ? e.message : 'Save failed');
                        break;
                    }
                    // The sink is already closed; drop it before settling so
                    // the shared cleanup does not abort a finalized stream.
                    downloadSinks.current.delete(complete.transfer_id);
                }
                settleTransfer(complete.transfer_id, { status: 'completed' });
                break;
            }
            case 'transfer_error': {
                const error = msg as TransferError;
                settleTransfer(error.transfer_id, {
                    status: 'error',
                    errorMessage: error.message,
                    errorCode: error.error_code,
                });
                break;
            }
        }
    }, [updateTransfer, settleTransfer, failDownload]);

    // Handle incoming DataChannel messages
    const setupDataChannelHandlers = useCallback((dc: RTCDataChannel) => {
        dc.binaryType = 'arraybuffer';

        dc.onmessage = (event) => {
            if (typeof event.data === 'string') {
                // JSON control message. Never awaited inside the event
                // callback; a rejecting handler must not become an
                // unhandled promise rejection.
                let msg: FileTransferMessage;
                try {
                    msg = JSON.parse(event.data);
                } catch {
                    return;
                }
                void handleControlMessage(msg).catch((e) => {
                    console.error('File transfer control handler error:', e);
                });
            } else {
                // Binary data chunk
                const parsed = parseBinaryChunk(event.data);
                if (!parsed) return;

                if (!activeTransfers.current.touch(parsed.transferId)) return;
                const meta = downloadMetas.current.get(parsed.transferId);
                const sink = downloadSinks.current.get(parsed.transferId);
                if (!meta || !sink) return;

                // Stream/queue the chunk; a write failure (disk full,
                // revoked permission) aborts the transfer and tells the
                // host to stop.
                sink.write(parsed.chunkIndex, parsed.chunkData).catch((e) => {
                    failDownload(parsed.transferId, e instanceof Error ? e.message : 'Write failed');
                });

                meta.receivedChunks++;
                meta.transferredBytes += parsed.chunkData.length;
                const progress = meta.totalChunks > 0
                    ? Math.round((meta.receivedChunks / meta.totalChunks) * 100)
                    : 100;
                const transferredBytes = meta.transferredBytes;

                // Throttle UI updates (always allow first update and last chunk)
                const speedState = transferSpeedState.current.get(parsed.transferId);
                const now = Date.now();
                const isLastChunk = meta.receivedChunks >= meta.totalChunks;
                const isFirstUpdate = speedState && speedState.emaSpeed === 0;
                if (isLastChunk || !speedState || isFirstUpdate || (now - speedState.lastUIUpdate) >= 300) {
                    if (speedState) speedState.lastUIUpdate = now;
                    const { speed, remainingSeconds } = computeSpeedInfo(parsed.transferId, transferredBytes, meta.fileSize);
                    updateTransfer(parsed.transferId, {
                        progress,
                        transferredBytes,
                        speed,
                        remainingSeconds,
                    });
                }
            }
        };

        dc.onerror = (event) => {
            console.error('File transfer data channel error:', event);
            // Wake any upload still waiting on the gate so it does not
            // hang the UI in "connecting" forever.
            acceptGate.current.rejectAll('Data channel error');
        };
    }, [updateTransfer, computeSpeedInfo, handleControlMessage, failDownload]);

    // --- Connection lifecycle ---

    /** Reject every request still waiting for a reply. Used when the session that
     * would have carried those replies goes away. */
    const rejectPendingRequests = useCallback((error: Error) => {
        for (const pending of pendingRequests.current.values()) {
            clearTimeout(pending.timeout);
            pending.reject(error);
        }
        pendingRequests.current.clear();
    }, []);

    /**
     * Close the data plane and only the data plane.
     *
     * Callbacks are unbound before closing so a peer connection being replaced
     * cannot fire into state that now belongs to its successor — replacing
     * without this is how a failed attempt used to leave an orphan behind.
     */
    const closeDataPlane = useCallback(() => {
        const dc = dcRef.current;
        dcRef.current = null;
        if (dc) {
            dc.onopen = null;
            dc.onmessage = null;
            dc.onerror = null;
            try {
                dc.close();
            } catch {
                // Already closed.
            }
        }
        const pc = pcRef.current;
        pcRef.current = null;
        if (pc) {
            pc.onicecandidate = null;
            pc.oniceconnectionstatechange = null;
            try {
                pc.close();
            } catch {
                // Already closed.
            }
        }
    }, []);

    /**
     * End the data-channel attempt, keeping the signaling session alive.
     *
     * This is the split that lets the page degrade instead of going dark: file
     * bytes become unavailable and say why, while listing and deletion carry on
     * over the session.
     */
    const failChannelAttempt = useCallback((error: Error) => {
        const diagnostics = diagnosticsRef.current;
        const pc = pcRef.current;
        diagnostics.noteStates(pc?.iceGatheringState ?? null, pc?.iceConnectionState ?? null);
        diagnostics.failStage('dataChannel');
        closeDataPlane();
        // Wake any upload parked on the gate; without the channel it will never
        // be accepted.
        acceptGate.current.rejectAll(error.message);
        setChannelStatus('failed');
        setChannelFailure({
            kind: isConnectionError(error) ? error.kind : null,
            errorCode: error instanceof SignalingError ? error.code : undefined,
            message: error.message,
            diagnostics: diagnostics.snapshot(),
        });
        const attempt = channelAttemptRef.current;
        if (attempt) {
            channelAttemptRef.current = null;
            if (attempt.timeout) clearTimeout(attempt.timeout);
            attempt.reject(error);
        }
    }, [closeDataPlane]);

    /**
     * End the signaling session and everything that depends on it.
     *
     * The data plane goes with it: without signaling there is no way to finish
     * or repair a peer connection, and no way to deliver a reply.
     */
    const teardownSession = useCallback((error: Error) => {
        if (retryTimerRef.current) {
            clearTimeout(retryTimerRef.current);
            retryTimerRef.current = null;
        }
        const socket = wsRef.current;
        wsRef.current = null;
        sessionRef.current = null;
        detachAndCloseSocket(socket);
        failChannelAttempt(error);
        rejectPendingRequests(error);
        const attempt = sessionAttemptRef.current;
        if (attempt) {
            sessionAttemptRef.current = null;
            if (attempt.timeout) clearTimeout(attempt.timeout);
            diagnosticsRef.current.failStage('session');
            attempt.reject(error);
        }
    }, [failChannelAttempt, rejectPendingRequests]);

    /**
     * The signaling session, establishing it if necessary.
     *
     * A session that is already up is reused, so repeated operations — and a
     * data-channel retry — never open a second socket. A socket that never
     * finished its handshake is discarded instead of reused: re-sending
     * `RequestRemoteAccess` over it would admit a second session on the host.
     */
    const ensureSession = useCallback((): Promise<SignalingSession> => {
        const live = sessionRef.current;
        if (live && live.ws.readyState === WebSocket.OPEN) return Promise.resolve(live);
        const inFlight = sessionAttemptRef.current;
        if (inFlight) return inFlight.promise;
        if (!deskId) {
            return Promise.reject(new ConnectionError('session-closed', 'No desk ID'));
        }

        // Whatever is left of a previous, unfinished session goes now — before a
        // replacement exists, so no orphan can outlive this call.
        detachAndCloseSocket(wsRef.current);
        wsRef.current = null;
        sessionRef.current = null;
        closeDataPlane();

        const attempt = createAttempt<SignalingSession>();
        sessionAttemptRef.current = attempt;
        remoteAccessRetryRef.current = 0;
        const diagnostics = diagnosticsRef.current;
        diagnostics.startStage('session');

        const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
        const host = window.location.host;
        const url = new URL(`${protocol}//${host}/api/desk/signaling`);
        url.searchParams.append('api_version', String(SIGNALING_API_VERSION));
        url.searchParams.append('build_number', '1');
        url.searchParams.append('commit_hash', '1');
        url.searchParams.append('operation_system', 'wasm');
        url.searchParams.append('remote_desk_type', 'browser');

        const ws = new WebSocket(url.toString());
        wsRef.current = ws;

        const settle = (init: RemoteAccessInit) => {
            if (sessionAttemptRef.current !== attempt) return;
            sessionAttemptRef.current = null;
            if (attempt.timeout) clearTimeout(attempt.timeout);
            diagnostics.endStage('session');
            const session: SignalingSession = { ws, init };
            sessionRef.current = session;
            attempt.resolve(session);
        };

        attempt.timeout = setTimeout(() => {
            if (sessionAttemptRef.current !== attempt) return;
            teardownSession(new ConnectionError('session-timeout', 'File manager session timed out'));
        }, SESSION_TIMEOUT_MS);

        // Start the file page session with its restricted grant context when present.
        // The trusted central validates the token and stamps the capability ceiling;
        // owner sessions omit it.
        const sendRemoteAccessRequest = () => {
            const grant = readSessionGrant(deskId);
            const signaling_data: {
                purpose: "file_manager";
                grant_session_id?: string;
                org_id?: number;
            } = {
                purpose: "file_manager",
            };
            if (grant?.grantSessionId) {
                signaling_data.grant_session_id = grant.grantSessionId;
            }
            if (!grant?.grantSessionId && orgId != null) {
                signaling_data.org_id = orgId;
            }
            ws.send(JSON.stringify({
                request_id: uuidv4(),
                signaling_type: SIGNALING_TYPE_CODE_REQUEST_REMOTE_ACCESS,
                signaling_data,
                to_connection_id: deskId,
            }));
        };

        ws.onopen = () => {
            if (wsRef.current !== ws) return;
            sendRemoteAccessRequest();
        };

        ws.onerror = (err) => {
            console.error("File transfer WS error:", err);
            if (wsRef.current !== ws) return;
            teardownSession(new ConnectionError('session-closed', 'WebSocket connection failed'));
        };

        ws.onclose = () => {
            if (wsRef.current !== ws) return;
            console.log("File transfer WS closed");
            teardownSession(new ConnectionError('session-closed', 'File manager connection closed'));
        };

        /**
         * Handle the reply to `RequestRemoteAccess`.
         *
         * A business failure arrives as an initialization frame carrying an error
         * state, and a frame rejected before its handler ran arrives as the
         * protocol-level `Error`. Neither used to be inspected, so both were
         * silently dropped and every refusal looked like a timeout.
         */
        const handleRemoteAccessReply = (signaling: any) => {
            const errorCode: number | undefined = signaling.response_state?.error_code;
            const message: string | undefined = signaling.response_state?.message;
            if (sessionAttemptRef.current !== attempt) {
                // The session is already up, so this belongs to something built on
                // top of it — a rejected offer or candidate — and must not take
                // browsing down with it. It fails the data channel if one is being
                // set up, and is otherwise only worth a log.
                if (signaling.signaling_type === SIGNALING_TYPE_CODE_ERROR) {
                    const failure = new SignalingError(
                        message || 'The host rejected the connection',
                        errorCode ?? 0,
                    );
                    if (channelAttemptRef.current) {
                        failChannelAttempt(failure);
                    } else {
                        console.warn('File transfer: unmatched error frame', errorCode, message);
                    }
                }
                return;
            }
            if (errorCode) {
                if (
                    errorCode === deskErrorCodeEnum.ACTION_NEED_RETRY
                    && remoteAccessRetryRef.current < REMOTE_ACCESS_RETRY_LIMIT
                ) {
                    // The host is waiting on its manager credential proof. Ask
                    // again shortly rather than failing the page.
                    remoteAccessRetryRef.current += 1;
                    retryTimerRef.current = setTimeout(() => {
                        retryTimerRef.current = null;
                        if (wsRef.current !== ws || ws.readyState !== WebSocket.OPEN) return;
                        sendRemoteAccessRequest();
                    }, REMOTE_ACCESS_RETRY_DELAY_MS);
                    return;
                }
                teardownSession(new SignalingError(message || 'Remote access was refused', errorCode));
                return;
            }
            if (signaling.signaling_type === SIGNALING_TYPE_CODE_ERROR) {
                // An error frame with no code: still a refusal, and must not be
                // mistaken for a successful initialization.
                teardownSession(new SignalingError(message || 'Remote access failed', 0));
                return;
            }
            settle((signaling.signaling_data ?? {}) as RemoteAccessInit);
        };

        ws.onmessage = async (event) => {
            if (wsRef.current !== ws) return;
            try {
                const signaling = JSON.parse(event.data);
                const { signaling_type, signaling_data } = signaling;
                const pending = pendingRequests.current.get(signaling.request_id);
                if (pending && signaling_type === pending.expectedResponseType) {
                    clearTimeout(pending.timeout);
                    pendingRequests.current.delete(signaling.request_id);
                    if (signaling.response_state?.error_code) {
                        pending.reject(new SignalingError(
                            signaling.response_state.message || "File operation failed",
                            signaling.response_state.error_code,
                        ));
                    } else {
                        pending.resolve(signaling_data);
                    }
                    return;
                }
                if (pending && signaling_type === SIGNALING_TYPE_CODE_ERROR) {
                    // A request refused before its own handler ran comes back as the
                    // protocol-level error rather than the request's response type.
                    // Failing the caller here is what stops it from waiting out its
                    // full timeout for a reply that has already arrived.
                    clearTimeout(pending.timeout);
                    pendingRequests.current.delete(signaling.request_id);
                    pending.reject(new SignalingError(
                        signaling.response_state?.message || "File operation failed",
                        signaling.response_state?.error_code ?? 0,
                    ));
                    return;
                }
                if (pending && (
                    signaling_type === SIGNALING_TYPE_CODE_FILES_LISTED
                    || signaling_type === SIGNALING_TYPE_CODE_FILE_DELETED
                    || signaling_type === SIGNALING_TYPE_CODE_SYSTEM_INFO_RETRIEVED
                )) {
                    console.error(
                        "Protocol error: signaling response type did not match pending request",
                        { requestId: signaling.request_id, signaling_type, expected: pending.expectedResponseType },
                    );
                    return;
                }

                if (
                    signaling_type === SIGNALING_TYPE_CODE_REMOTE_ACCESS_INITIALIZED
                    || signaling_type === SIGNALING_TYPE_CODE_ERROR
                ) {
                    handleRemoteAccessReply(signaling);
                } else if (signaling_type === SIGNALING_TYPE_CODE_ANSWER) {
                    const pc = pcRef.current;
                    if (pc) {
                        await pc.setRemoteDescription(new RTCSessionDescription(signaling_data));
                    }
                } else if (signaling_type === SIGNALING_TYPE_CODE_ICE_CANDIDATE) {
                    const pc = pcRef.current;
                    if (
                        pc
                        && signaling_data.connection_epoch
                            === sessionRef.current?.init?.connection_epoch
                    ) {
                        await pc.addIceCandidate(new RTCIceCandidate(signaling_data.candidate));
                    }
                }
            } catch (e) {
                console.error("File transfer signaling error:", e);
            }
        };

        return attempt.promise;
    }, [deskId, orgId, closeDataPlane, teardownSession, failChannelAttempt]);

    /** Send a signaling frame over an established session. */
    const sendSignaling = useCallback((
        ws: WebSocket,
        signalingType: number,
        signalingData: unknown,
    ) => {
        if (ws.readyState !== WebSocket.OPEN) return;
        ws.send(JSON.stringify({
            request_id: uuidv4(),
            signaling_type: signalingType,
            signaling_data: signalingData,
            to_connection_id: deskId,
        }));
    }, [deskId]);

    /**
     * The file-transfer data channel, establishing it if necessary.
     *
     * Only file bytes come through here; everything else rides the session, so a
     * failure at this stage disables transfers without taking the page with it.
     */
    const ensureDataChannel = useCallback(async (): Promise<RTCDataChannel> => {
        const open = dcRef.current;
        if (open && open.readyState === 'open') return open;
        const inFlight = channelAttemptRef.current;
        if (inFlight) return inFlight.promise;

        const attempt = createAttempt<RTCDataChannel>();
        channelAttemptRef.current = attempt;
        setChannelStatus('connecting');
        setChannelFailure(null);
        const diagnostics = diagnosticsRef.current;
        // A retry reports its own candidates, not the previous attempt's.
        diagnostics.resetDataChannel();
        diagnostics.startStage('dataChannel');

        attempt.timeout = setTimeout(() => {
            if (channelAttemptRef.current !== attempt) return;
            failChannelAttempt(new ConnectionError('channel-timeout', 'File transfer channel timed out'));
        }, DATA_CHANNEL_TIMEOUT_MS);

        try {
            const session = await ensureSession();
            // A newer attempt (or a teardown) took over while the session was
            // being established; this one no longer owns anything.
            if (channelAttemptRef.current !== attempt) return attempt.promise;

            // Replace, never stack.
            closeDataPlane();

            const iceServers = (session.init.ice_servers ?? []) as RTCIceServer[];
            diagnostics.noteIceServers(iceServers);
            const epoch = session.init.connection_epoch;
            const pc = new RTCPeerConnection({ iceServers });
            pcRef.current = pc;

            const dc = pc.createDataChannel('file_transfer_event', { ordered: true });
            dcRef.current = dc;
            setupDataChannelHandlers(dc);

            dc.onopen = () => {
                console.log("File transfer data channel open");
                if (channelAttemptRef.current !== attempt) return;
                channelAttemptRef.current = null;
                if (attempt.timeout) clearTimeout(attempt.timeout);
                diagnostics.endStage('dataChannel');
                diagnostics.noteStates(pc.iceGatheringState ?? null, pc.iceConnectionState ?? null);
                setChannelStatus('ready');
                setChannelFailure(null);
                attempt.resolve(dc);
            };

            pc.onicecandidate = (event) => {
                if (pcRef.current !== pc) return;
                const candidate = event.candidate;
                if (!candidate) {
                    // End of gathering. Nothing is sent for it: the offer and every
                    // candidate have already gone out as they were produced.
                    diagnostics.noteStates(pc.iceGatheringState ?? 'complete', null);
                    return;
                }
                diagnostics.noteCandidate(candidate.candidate);
                sendSignaling(session.ws, SIGNALING_TYPE_CODE_ICE_CANDIDATE, {
                    connection_epoch: epoch,
                    candidate: candidate.toJSON(),
                });
            };

            pc.oniceconnectionstatechange = () => {
                if (pcRef.current !== pc) return;
                diagnostics.noteStates(pc.iceGatheringState ?? null, pc.iceConnectionState);
                if (pc.iceConnectionState === 'failed') {
                    failChannelAttempt(new ConnectionError('ice-failed', 'ICE negotiation failed'));
                }
            };

            const offer = await pc.createOffer();
            await pc.setLocalDescription(offer);
            if (pcRef.current !== pc) return attempt.promise;

            // Trickle ICE: the offer goes out now and candidates follow as they are
            // gathered. Holding it back until gathering completed made every slow or
            // unreachable ICE server fatal, because gathering does not finish until
            // each configured server's allocation attempt has timed out.
            sendSignaling(session.ws, SIGNALING_TYPE_CODE_OFFER, {
                offer: pc.localDescription,
                connection_epoch: epoch,
                // DataChannel-only offer carries an explicit null session-settings
                // field by protocol.
                session_settings: null,
            });
        } catch (error) {
            if (channelAttemptRef.current === attempt) {
                failChannelAttempt(
                    error instanceof Error
                        ? error
                        : new ConnectionError('channel-closed', 'File transfer channel failed'),
                );
            }
        }
        return attempt.promise;
    }, [ensureSession, closeDataPlane, failChannelAttempt, setupDataChannelHandlers, sendSignaling]);

    /**
     * Warm the data channel in the background.
     *
     * Transfers do not need it until the user asks for one, but finding out it is
     * broken only at that point would hide the failure behind a click and make
     * the first transfer pay the whole ICE cost. The rejection is deliberately
     * swallowed: the outcome is reported through `channelStatus` /
     * `channelFailure`, not by throwing at a caller that is not waiting.
     */
    const prepareTransfers = useCallback(() => {
        void ensureDataChannel().catch(() => { });
    }, [ensureDataChannel]);

    const sendFileRequest = useCallback(async <T,>(
        signalingType: number,
        expectedResponseType: number,
        signalingData: unknown,
    ): Promise<T> => {
        // Only the session is required. These requests travel over the WebSocket,
        // and the host admits the connection when it answers `RequestRemoteAccess`
        // — long before any peer connection exists.
        const session = await ensureSession();
        const ws = session.ws;
        if (!deskId || ws.readyState !== WebSocket.OPEN) {
            throw new ConnectionError('session-closed', "File manager connection is not open");
        }
        const requestId = uuidv4();
        return new Promise<T>((resolve, reject) => {
            const timeout = setTimeout(() => {
                pendingRequests.current.delete(requestId);
                reject(new ConnectionError('session-closed', "File operation timed out"));
            }, 30_000);
            pendingRequests.current.set(requestId, {
                resolve: value => resolve(value as T),
                reject,
                timeout,
                expectedResponseType,
            });
            try {
                ws.send(JSON.stringify({
                    request_id: requestId,
                    signaling_type: signalingType,
                    signaling_data: signalingData,
                    to_connection_id: deskId,
                }));
            } catch (error) {
                clearTimeout(timeout);
                pendingRequests.current.delete(requestId);
                reject(error instanceof Error ? error : new Error("File request send failed"));
            }
        });
    }, [deskId, ensureSession]);

    const listFiles = useCallback((params: unknown) => (
        sendFileRequest<any>(SIGNALING_TYPE_CODE_LIST_FILES, SIGNALING_TYPE_CODE_FILES_LISTED, params)
    ), [sendFileRequest]);

    const deleteFile = useCallback((request: unknown) => (
        sendFileRequest<void>(SIGNALING_TYPE_CODE_DELETE_FILE, SIGNALING_TYPE_CODE_FILE_DELETED, request)
    ), [sendFileRequest]);

    // The host's own system information. The server this browser is connected to
    // may be a manager or a signaling server sitting between it and the host, so
    // its `/api/desk/sysinfo` describes the wrong machine; only the host can say
    // what it is. Rejects like any other request — callers that only use this to
    // decorate the UI should treat a failure as "unknown" rather than an error.
    //
    // `Partial`: the response is the shared signaling document, whose fields a
    // host fills as far as it can, so a reader must handle any of them missing.
    const querySystemInfo = useCallback(() => (
        sendFileRequest<Partial<SystemInfo>>(
            SIGNALING_TYPE_CODE_GET_SYSTEM_INFO,
            SIGNALING_TYPE_CODE_SYSTEM_INFO_RETRIEVED,
            null,
        )
    ), [sendFileRequest]);

    // Close WebRTC and WebSocket connections. Idempotent: a second call finds
    // nothing left to settle and must not reject anything twice.
    const closeConnection = useCallback(() => {
        // End every transfer still in flight. Without this the connection went
        // away while their sinks stayed open, their watchdogs stayed armed and
        // their rows stayed on screen mid-progress.
        for (const transferId of activeTransfers.current.settleAll()) {
            void cleanupDownload(transferId);
            updateTransfer(transferId, {
                status: 'error',
                errorMessage: 'File manager connection closed',
            });
            removeTransferAfterDelay(transferId);
        }
        // Wake every pending upload waiter before tearing down.
        acceptGate.current.rejectAll('Connection closed');
        const closed = new ConnectionError('session-closed', "File manager connection closed");
        rejectPendingRequests(closed);
        if (retryTimerRef.current) {
            clearTimeout(retryTimerRef.current);
            retryTimerRef.current = null;
        }
        const channelAttempt = channelAttemptRef.current;
        if (channelAttempt) {
            channelAttemptRef.current = null;
            if (channelAttempt.timeout) clearTimeout(channelAttempt.timeout);
            channelAttempt.reject(closed);
        }
        const sessionAttempt = sessionAttemptRef.current;
        if (sessionAttempt) {
            sessionAttemptRef.current = null;
            if (sessionAttempt.timeout) clearTimeout(sessionAttempt.timeout);
            sessionAttempt.reject(closed);
        }
        const ws = wsRef.current;
        if (pcRef.current && ws && ws.readyState === WebSocket.OPEN && deskId) {
            // Send close control
            ws.send(JSON.stringify({
                request_id: uuidv4(),
                signaling_type: SIGNALING_TYPE_CODE_CLOSE_REMOTE_SESSION,
                signaling_data: {
                    connection_epoch: sessionRef.current?.init?.connection_epoch,
                    finalize_logical_connection: true,
                },
                to_connection_id: deskId,
            }));
        }
        closeDataPlane();
        wsRef.current = null;
        sessionRef.current = null;
        detachAndCloseSocket(ws);
        setChannelStatus('idle');
        setChannelFailure(null);
    }, [deskId, cleanupDownload, updateTransfer, removeTransferAfterDelay, rejectPendingRequests, closeDataPlane]);

    // Download a file
    const downloadFile = useCallback(async (filePath: string, fileName: string) => {
        const transferId = uuidv4();

        // Open the destination stream first, while the click's user
        // activation is still valid. Streaming straight to disk keeps
        // peak memory at ~one chunk regardless of file size.
        let writable: WritableFileStreamLike | null = null;
        if (canStreamToDisk()) {
            writable = await openStreamingWritable(fileName);
            if (!writable) {
                // User cancelled the save dialog — abandon silently.
                return;
            }
            downloadSinks.current.set(transferId, new StreamingDownloadSink(writable));
        }

        // Add transfer to list
        activeTransfers.current.start(transferId);
        setTransfers(prev => [...prev, {
            transferId,
            fileName,
            fileSize: 0,
            direction: 'download',
            status: 'connecting',
            progress: 0,
            transferredBytes: 0,
            speed: 0,
            remainingSeconds: 0,
        }]);
        transferSpeedState.current.set(transferId, { startTime: Date.now(), lastCalcTime: Date.now(), lastCalcBytes: 0, lastUIUpdate: Date.now(), emaSpeed: 0 });

        try {
            const dc = await ensureDataChannel();
            const request: DownloadRequest = {
                type: 'download_request',
                transfer_id: transferId,
                file_path: filePath,
            };
            dc.send(JSON.stringify(request));
            updateTransfer(transferId, { status: 'transferring' });
            // Nothing in the protocol announces a host that refuses the
            // request, answers and then stops sending, or stalls mid-file.
            // From here on, silence itself ends the transfer.
            activeTransfers.current.watch(transferId, () => {
                failDownload(
                    transferId,
                    'The host stopped responding',
                    deskErrorCodeEnum.TIMEOUT,
                );
            });
        } catch (err) {
            settleTransfer(transferId, {
                status: 'error',
                errorMessage: err instanceof Error ? err.message : 'Connection failed',
                errorCode: err instanceof SignalingError || isConnectionError(err) ? err.code : undefined,
            });
        }
    }, [ensureDataChannel, updateTransfer, settleTransfer, failDownload]);

    // Upload a file
    const uploadFile = useCallback(async (targetDir: string, file: File) => {
        const transferId = uuidv4();

        activeTransfers.current.start(transferId);
        setTransfers(prev => [...prev, {
            transferId,
            fileName: file.name,
            fileSize: file.size,
            direction: 'upload',
            status: 'connecting',
            progress: 0,
            transferredBytes: 0,
            speed: 0,
            remainingSeconds: 0,
        }]);
        transferSpeedState.current.set(transferId, { startTime: Date.now(), lastCalcTime: Date.now(), lastCalcBytes: 0, lastUIUpdate: Date.now(), emaSpeed: 0 });

        try {
            const dc = await ensureDataChannel();

            const chunkSize = FILE_TRANSFER_CHUNK_SIZE;
            // Exactly the number of chunks the loop below will send. An empty
            // file sends none, and claiming one anyway made the host wait for
            // a chunk that never came and then reject the finished upload as
            // incomplete. The Android and iOS clients already declare zero.
            const totalChunks = Math.ceil(file.size / chunkSize);

            // Send upload request
            const request: UploadRequest = {
                type: 'upload_request',
                transfer_id: transferId,
                target_dir: targetDir,
                file_name: file.name,
                file_size: file.size,
                chunk_size: chunkSize,
                total_chunks: totalChunks,
            };
            dc.send(JSON.stringify(request));

            // Wait for the host to accept (open the destination file)
            // before pushing any bytes. Refusal, cancel, disconnect or
            // timeout reject this and skip the chunk loop entirely.
            await acceptGate.current.wait(transferId);

            const reader = file.stream().getReader();
            let chunkIndex = 0;
            let sentBytes = 0;
            let lastUploadUpdate = Date.now();
            let leftover = new Uint8Array(0);

            while (true) {
                // Check if this upload has been cancelled
                // Leaving the registry is what cancellation means: whoever
                // ended this transfer — the user, the host, a disconnect —
                // settled it there.
                if (!activeTransfers.current.isActive(transferId)) {
                    reader.cancel();
                    return;
                }

                const { done, value } = await reader.read();

                // Combine leftover with new data
                let combined: Uint8Array;
                if (leftover.length > 0 && value) {
                    combined = new Uint8Array(leftover.length + value.length);
                    combined.set(leftover);
                    combined.set(value, leftover.length);
                    leftover = new Uint8Array(0);
                } else if (value) {
                    combined = value;
                } else {
                    combined = leftover;
                    leftover = new Uint8Array(0);
                }

                // Send full chunks
                let offset = 0;
                while (offset + chunkSize <= combined.length) {
                    // Check cancellation before each chunk send
                    if (!activeTransfers.current.isActive(transferId)) {
                        reader.cancel();
                        return;
                    }

                    const chunk = combined.slice(offset, offset + chunkSize);
                    const buf = buildBinaryChunk(transferId, chunkIndex, chunk);
                    dc.send(buf);
                    chunkIndex++;
                    sentBytes += chunk.length;
                    offset += chunkSize;

                    // Throttle UI updates to max once per 500ms
                    const now = Date.now();
                    if (now - lastUploadUpdate >= 500) {
                        const progress = Math.round((sentBytes / file.size) * 100);
                        const { speed, remainingSeconds } = computeSpeedInfo(transferId, sentBytes, file.size);
                        updateTransfer(transferId, { progress, transferredBytes: sentBytes, speed, remainingSeconds });
                        lastUploadUpdate = now;
                    }

                    // Robust backpressure: wait if browser send buffer is too full
                    if (dc.bufferedAmount > 2 * 1024 * 1024) {
                        while (dc.bufferedAmount > 512 * 1024) {
                            await new Promise(r => setTimeout(r, 100));
                        }
                    }
                }

                // Save leftover
                if (offset < combined.length) {
                    leftover = combined.slice(offset);
                }

                if (done) {
                    // Send remaining leftover as last chunk
                    if (leftover.length > 0) {
                        const buf = buildBinaryChunk(transferId, chunkIndex, leftover);
                        dc.send(buf);
                        sentBytes += leftover.length;
                        chunkIndex++;
                    }
                    break;
                }
            }

            // Send transfer complete
            const complete: TransferComplete = {
                type: 'transfer_complete',
                transfer_id: transferId,
            };
            dc.send(JSON.stringify(complete));

            // Sending the last chunk is not the same as the host having kept
            // the file: it still verifies the byte count and can answer
            // `transfer_error`. Show success, but stay registered so that
            // verdict is not discarded as an unknown id — and let silence
            // release the entry if the host never answers at all.
            updateTransfer(transferId, {
                status: 'completed',
                progress: 100,
                transferredBytes: file.size,
            });
            removeTransferAfterDelay(transferId, 60000);
            // Releasing the entry is the callback's job — the registry keeps it
            // registered while the callback runs precisely so the release goes
            // through the same single exit as every other ending. An empty
            // callback would leave the entry live forever with its timer spent,
            // which is both a leak and a way for a late reply to be accepted
            // long after this upload stopped meaning anything. No outcome: the
            // row already reads "completed" and silence is not a reason to
            // contradict it.
            activeTransfers.current.watch(transferId, () => {
                settleTransfer(transferId);
            });

        } catch (err) {
            settleTransfer(transferId, {
                status: 'error',
                errorMessage: err instanceof Error ? err.message : 'Upload failed',
                errorCode: err instanceof SignalingError || isConnectionError(err) ? err.code : undefined,
            });
        }
    }, [ensureDataChannel, updateTransfer, computeSpeedInfo, removeTransferAfterDelay, settleTransfer]);

    // Cancel an active transfer (download or upload)
    const cancelTransfer = useCallback((transferId: string) => {
        // Send cancel message to server
        sendControlBestEffort({ type: 'transfer_cancel', transfer_id: transferId });
        settleTransfer(transferId, { status: 'error', errorMessage: 'Cancelled' });
    }, [sendControlBestEffort, settleTransfer]);

    // Manually remove a transfer from the list
    const removeTransfer = useCallback((transferId: string) => {
        settleTransfer(transferId);
        setTransfers(prev => prev.filter(t => t.transferId !== transferId));
    }, [settleTransfer]);

    return {
        transfers,
        downloadFile,
        uploadFile,
        cancelTransfer,
        removeTransfer,
        listFiles,
        deleteFile,
        querySystemInfo,
        closeConnection,
        prepareTransfers,
        channelStatus,
        channelFailure,
    };
}
