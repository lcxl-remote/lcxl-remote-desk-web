import { useRef, useState, useCallback } from 'react';
import { v4 as uuidv4 } from 'uuid';
import {
    SIGNALING_TYPE_CODE_REQUEST_REMOTE,
    SIGNALING_TYPE_CODE_INIT,
    SIGNALING_TYPE_CODE_OFFER,
    SIGNALING_TYPE_CODE_ANSWER,
    SIGNALING_TYPE_CODE_CANID,
    SIGNALING_TYPE_CODE_MANAGER_FILE_LIST,
    SIGNALING_TYPE_CODE_MANAGER_FILE_DELETE,
    SIGNALING_TYPE_CODE_CLOSE_CONTROL,
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
import { deskErrorCodeEnum } from '@/services/types';

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


export function useFileTransfer(deskId: string | undefined) {
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
    const connectPromiseRef = useRef<{
        promise: Promise<RTCDataChannel>;
        resolve: (dc: RTCDataChannel) => void;
        reject: (err: Error) => void;
        timeout?: ReturnType<typeof setTimeout>;
    } | null>(null);
    // Store init data received from signaling
    const initDataRef = useRef<any>(null);
    const pendingRequests = useRef(new Map<string, {
        resolve: (value: any) => void;
        reject: (error: Error) => void;
        timeout: ReturnType<typeof setTimeout>;
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

    // Establish WebRTC connection via signaling, return data channel
    const ensureConnection = useCallback(async (): Promise<RTCDataChannel> => {
        // Already connected
        if (dcRef.current && dcRef.current.readyState === 'open') {
            return dcRef.current;
        }

        if (!deskId) throw new Error('No desk ID');

        const existingAttempt = connectPromiseRef.current;
        if (existingAttempt) return existingAttempt.promise;

        let resolveAttempt!: (dc: RTCDataChannel) => void;
        let rejectAttempt!: (error: Error) => void;
        const promise = new Promise<RTCDataChannel>((resolve, reject) => {
            resolveAttempt = resolve;
            rejectAttempt = reject;
        });
        const attempt: NonNullable<typeof connectPromiseRef.current> = {
            promise,
            resolve: resolveAttempt,
            reject: rejectAttempt,
        };
        connectPromiseRef.current = attempt;
        attempt.timeout = setTimeout(() => {
            if (connectPromiseRef.current !== attempt) return;
            connectPromiseRef.current = null;
            attempt.reject(new Error('File manager connection timed out'));
            wsRef.current?.close();
        }, 20_000);

        // 1. Connect WebSocket
            const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
            const host = window.location.host;
            const url = new URL(`${protocol}//${host}/api/desk/signaling`);
            url.searchParams.append('api_version', '1');
            url.searchParams.append('build_number', '1');
            url.searchParams.append('commit_hash', '1');
            url.searchParams.append('operation_system', 'wasm');
            url.searchParams.append('remote_desk_type', 'browser');

            const ws = new WebSocket(url.toString());
            wsRef.current = ws;

            ws.onopen = () => {
                if (wsRef.current !== ws) return;
                // Start the file page session with its restricted grant context when present.
                // The trusted central validates the token and stamps the capability ceiling;
                // owner sessions omit it.
                const grant = readSessionGrant(deskId);
                const signaling_data: { purpose: "file_manager"; grant_session_id?: string } = {
                    purpose: "file_manager",
                };
                if (grant?.grantSessionId) {
                    signaling_data.grant_session_id = grant.grantSessionId;
                }
                const msg = {
                    request_id: uuidv4(),
                    signaling_type: SIGNALING_TYPE_CODE_REQUEST_REMOTE,
                    signaling_data,
                    to_connection_id: deskId,
                };
                ws.send(JSON.stringify(msg));
            };

            ws.onerror = (err) => {
                console.error("File transfer WS error:", err);
                if (connectPromiseRef.current === attempt) {
                    if (attempt.timeout) clearTimeout(attempt.timeout);
                    connectPromiseRef.current = null;
                    attempt.reject(new Error("WebSocket connection failed"));
                    ws.close();
                }
            };

            ws.onclose = () => {
                if (wsRef.current !== ws) return;
                wsRef.current = null;
                console.log("File transfer WS closed");
                if (connectPromiseRef.current === attempt) {
                    if (attempt.timeout) clearTimeout(attempt.timeout);
                    connectPromiseRef.current = null;
                    attempt.reject(new Error("File manager connection closed"));
                }
                for (const pending of pendingRequests.current.values()) {
                    clearTimeout(pending.timeout);
                    pending.reject(new Error("File manager connection closed"));
                }
                pendingRequests.current.clear();
                dcRef.current?.close();
                dcRef.current = null;
                pcRef.current?.close();
                pcRef.current = null;
            };

            ws.onmessage = async (event) => {
                if (wsRef.current !== ws) return;
                try {
                    const signaling = JSON.parse(event.data);
                    const { signaling_type, signaling_data } = signaling;
                    const pending = pendingRequests.current.get(signaling.request_id);
                    if (pending && (
                        signaling_type === SIGNALING_TYPE_CODE_MANAGER_FILE_LIST
                        || signaling_type === SIGNALING_TYPE_CODE_MANAGER_FILE_DELETE
                    )) {
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

                    if (signaling_type === SIGNALING_TYPE_CODE_INIT) {
                        initDataRef.current = signaling_data;
                        // 2. Create RTCPeerConnection
                        const pc = new RTCPeerConnection({
                            iceServers: signaling_data.ice_servers || [],
                        });
                        pcRef.current = pc;

                        // 3. Create data channel (only file transfer, no video/audio)
                        const dc = pc.createDataChannel('file_transfer_event', { ordered: true });
                        dcRef.current = dc;

                        setupDataChannelHandlers(dc);

                        dc.onopen = () => {
                            console.log("File transfer data channel open");
                            if (connectPromiseRef.current === attempt) {
                                if (attempt.timeout) clearTimeout(attempt.timeout);
                                connectPromiseRef.current = null;
                                attempt.resolve(dc);
                            }
                        };

                        pc.onicecandidate = (event) => {
                            if (event.candidate === null && pc.localDescription) {
                                // Send OFFER with minimal desk settings
                                const offerModel = {
                                    offer: pc.localDescription,
                                    desk_settings: signaling_data.desk_settings,
                                };
                                const msg = {
                                    request_id: uuidv4(),
                                    signaling_type: SIGNALING_TYPE_CODE_OFFER,
                                    signaling_data: offerModel,
                                    to_connection_id: deskId,
                                };
                                ws.send(JSON.stringify(msg));
                            }
                        };

                        pc.oniceconnectionstatechange = () => {
                            if (pc.iceConnectionState === 'failed' || pc.iceConnectionState === 'disconnected') {
                                console.warn('File transfer ICE connection state:', pc.iceConnectionState);
                            }
                        };

                        // Create offer
                        const offer = await pc.createOffer();
                        await pc.setLocalDescription(offer);

                    } else if (signaling_type === SIGNALING_TYPE_CODE_ANSWER) {
                        const pc = pcRef.current;
                        if (pc) {
                            await pc.setRemoteDescription(new RTCSessionDescription(signaling_data));
                        }
                    } else if (signaling_type === SIGNALING_TYPE_CODE_CANID) {
                        const pc = pcRef.current;
                        if (pc) {
                            await pc.addIceCandidate(new RTCIceCandidate(signaling_data));
                        }
                    }
                } catch (e) {
                    console.error("File transfer signaling error:", e);
                    if (connectPromiseRef.current === attempt) {
                        if (attempt.timeout) clearTimeout(attempt.timeout);
                        connectPromiseRef.current = null;
                        attempt.reject(e instanceof Error ? e : new Error("File manager signaling failed"));
                        ws.close();
                    }
                }
            };
        return promise;
    }, [deskId, setupDataChannelHandlers]);

    const sendFileRequest = useCallback(async <T,>(
        signalingType: number,
        signalingData: unknown,
    ): Promise<T> => {
        await ensureConnection();
        const ws = wsRef.current;
        if (!deskId || !ws || ws.readyState !== WebSocket.OPEN) {
            throw new Error("File manager connection is not open");
        }
        const requestId = uuidv4();
        return new Promise<T>((resolve, reject) => {
            const timeout = setTimeout(() => {
                pendingRequests.current.delete(requestId);
                reject(new Error("File operation timed out"));
            }, 30_000);
            pendingRequests.current.set(requestId, {
                resolve: value => resolve(value as T),
                reject,
                timeout,
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
    }, [deskId, ensureConnection]);

    const listFiles = useCallback((params: unknown) => (
        sendFileRequest<any>(SIGNALING_TYPE_CODE_MANAGER_FILE_LIST, params)
    ), [sendFileRequest]);

    const deleteFile = useCallback((request: unknown) => (
        sendFileRequest<void>(SIGNALING_TYPE_CODE_MANAGER_FILE_DELETE, request)
    ), [sendFileRequest]);

    // Close WebRTC and WebSocket connections
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
        for (const pending of pendingRequests.current.values()) {
            clearTimeout(pending.timeout);
            pending.reject(new Error("File manager connection closed"));
        }
        pendingRequests.current.clear();
        const attempt = connectPromiseRef.current;
        if (attempt) {
            if (attempt.timeout) clearTimeout(attempt.timeout);
            connectPromiseRef.current = null;
            attempt.reject(new Error("File manager connection closed"));
        }
        if (dcRef.current) {
            dcRef.current.close();
            dcRef.current = null;
        }
        if (pcRef.current) {
            // Send close control
            if (wsRef.current && wsRef.current.readyState === WebSocket.OPEN && deskId) {
                const msg = {
                    request_id: uuidv4(),
                    signaling_type: SIGNALING_TYPE_CODE_CLOSE_CONTROL,
                    signaling_data: null,
                    to_connection_id: deskId,
                };
                wsRef.current.send(JSON.stringify(msg));
            }
            pcRef.current.close();
            pcRef.current = null;
        }
        if (wsRef.current) {
            wsRef.current.close();
            wsRef.current = null;
        }
    }, [deskId]);

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
            const dc = await ensureConnection();
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
            });
        }
    }, [ensureConnection, updateTransfer, settleTransfer, failDownload]);

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
            const dc = await ensureConnection();

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
            activeTransfers.current.watch(transferId, () => {});

        } catch (err) {
            settleTransfer(transferId, {
                status: 'error',
                errorMessage: err instanceof Error ? err.message : 'Upload failed',
            });
        }
    }, [ensureConnection, updateTransfer, computeSpeedInfo, removeTransferAfterDelay, settleTransfer]);

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
        closeConnection,
    };
}
