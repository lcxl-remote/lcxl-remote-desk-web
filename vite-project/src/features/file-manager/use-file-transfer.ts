import { useRef, useState, useCallback } from 'react';
import { v4 as uuidv4 } from 'uuid';
import {
    SIGNALING_TYPE_CODE_REQUEST_REMOTE,
    SIGNALING_TYPE_CODE_INIT,
    SIGNALING_TYPE_CODE_OFFER,
    SIGNALING_TYPE_CODE_ANSWER,
    SIGNALING_TYPE_CODE_CANID,
    SIGNALING_TYPE_CODE_CLOSE_CONTROL,
} from '../desk/constants';

// Per-chunk DC payload size for uploads (browser → host). Must stay in
// sync with `FILE_TRANSFER_CHUNK_SIZE_TX` in the Rust dispatcher
// (`worker/file_transfer_dispatcher.rs`). Raised to 240 KiB on
// 2026-05-11 after metrics showed `dc.send` per-message overhead
// dominating throughput at the previous 60 KB. NOT 256 KiB because
// the wire-level SCTP message is `payload + 40-byte header`, and a
// 256 KiB payload yields a 262184-byte message that just barely
// exceeds Chrome's typical `a=max-message-size:262144` SDP advertise
// (host-side webrtc-sctp rejects with ErrOutboundPacketTooLarge).
// 240 KiB leaves ~16 KiB of headroom for the header plus any future
// protocol expansion.
const FILE_TRANSFER_CHUNK_SIZE = 240 * 1024;

const BINARY_HEADER_SIZE = 36 + 4; // UUID (36) + chunk_index (4)

// --- Protocol types ---

interface DownloadRequest {
    type: 'download_request';
    transfer_id: string;
    file_path: string;
}

interface DownloadResponse {
    type: 'download_response';
    transfer_id: string;
    file_name: string;
    file_size: number;
    chunk_size: number;
    total_chunks: number;
}

interface UploadRequest {
    type: 'upload_request';
    transfer_id: string;
    target_dir: string;
    file_name: string;
    file_size: number;
    chunk_size: number;
    total_chunks: number;
}

interface UploadResponse {
    type: 'upload_response';
    transfer_id: string;
    accepted: boolean;
    message?: string;
}

interface TransferComplete {
    type: 'transfer_complete';
    transfer_id: string;
}

interface TransferError {
    type: 'transfer_error';
    transfer_id: string;
    message: string;
}

interface TransferCancel {
    type: 'transfer_cancel';
    transfer_id: string;
}

type FileTransferMessage =
    | DownloadRequest
    | DownloadResponse
    | UploadRequest
    | UploadResponse
    | TransferComplete
    | TransferError
    | TransferCancel;

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
}

// --- Internal state for active downloads ---
interface DownloadState {
    fileName: string;
    fileSize: number;
    totalChunks: number;
    receivedChunks: number;
    transferredBytes: number;
    chunks: Uint8Array[];
}


// --- File save utilities ---

/** Try to save a file using File System Access API (user picks location). Returns true if saved. */
async function saveFileWithPicker(blob: Blob, fileName: string): Promise<boolean> {
    if (!('showSaveFilePicker' in window)) return false;
    try {
        const handle = await (window as any).showSaveFilePicker({
            suggestedName: fileName,
        });
        const writable = await handle.createWritable();
        await writable.write(blob);
        await writable.close();
        return true;
    } catch {
        // User cancelled the dialog or API error
        return false;
    }
}

/** Fallback: trigger a browser download to the default download directory. */
function triggerBrowserDownload(blob: Blob, fileName: string) {
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = fileName;
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
    URL.revokeObjectURL(url);
}

export function useFileTransfer(deskId: string | undefined) {
    const wsRef = useRef<WebSocket | null>(null);
    const pcRef = useRef<RTCPeerConnection | null>(null);
    const dcRef = useRef<RTCDataChannel | null>(null);
    const [transfers, setTransfers] = useState<TransferProgress[]>([]);
    const downloadStates = useRef<Map<string, DownloadState>>(new Map());
    const cancelledTransfers = useRef<Set<string>>(new Set());
    // Track transfer speed state for EMA smoothing
    const transferSpeedState = useRef<Map<string, {
        startTime: number;
        lastCalcTime: number;
        lastCalcBytes: number;
        lastUIUpdate: number;
        emaSpeed: number;
    }>>(new Map());
    const connectPromiseRef = useRef<{
        resolve: (dc: RTCDataChannel) => void;
        reject: (err: Error) => void;
    } | null>(null);
    // Store init data received from signaling
    const initDataRef = useRef<any>(null);

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

    // Parse binary chunk: transfer_id (36 bytes) + chunk_index (4 bytes BE) + data
    const parseBinaryChunk = useCallback((data: ArrayBuffer): { transferId: string; chunkIndex: number; chunkData: Uint8Array } | null => {
        if (data.byteLength < BINARY_HEADER_SIZE) return null;
        const view = new DataView(data);
        const decoder = new TextDecoder('utf-8');
        const transferId = decoder.decode(new Uint8Array(data, 0, 36));
        const chunkIndex = view.getUint32(36, false); // big-endian
        const chunkData = new Uint8Array(data, BINARY_HEADER_SIZE);
        return { transferId, chunkIndex, chunkData };
    }, []);

    // Build binary chunk with header
    const buildBinaryChunk = useCallback((transferId: string, chunkIndex: number, data: Uint8Array): ArrayBuffer => {
        const encoder = new TextEncoder();
        const idBytes = encoder.encode(transferId); // should be 36 bytes
        const buf = new ArrayBuffer(BINARY_HEADER_SIZE + data.length);
        const view = new DataView(buf);
        new Uint8Array(buf).set(idBytes, 0);
        view.setUint32(36, chunkIndex, false); // big-endian
        new Uint8Array(buf, BINARY_HEADER_SIZE).set(data);
        return buf;
    }, []);

    // Handle incoming DataChannel messages
    const setupDataChannelHandlers = useCallback((dc: RTCDataChannel) => {
        dc.binaryType = 'arraybuffer';

        dc.onmessage = (event) => {
            if (typeof event.data === 'string') {
                // JSON control message
                const msg: FileTransferMessage = JSON.parse(event.data);
                handleControlMessage(msg);
            } else {
                // Binary data chunk
                const parsed = parseBinaryChunk(event.data);
                if (!parsed) return;

                const state = downloadStates.current.get(parsed.transferId);
                if (!state) return;

                state.chunks[parsed.chunkIndex] = parsed.chunkData;
                state.receivedChunks++;
                state.transferredBytes += parsed.chunkData.length;

                const progress = Math.round((state.receivedChunks / state.totalChunks) * 100);
                const transferredBytes = state.transferredBytes;


                // Throttle UI updates (always allow first update and last chunk)
                const speedState = transferSpeedState.current.get(parsed.transferId);
                const now = Date.now();
                const isLastChunk = state.receivedChunks >= state.totalChunks;
                const isFirstUpdate = speedState && speedState.emaSpeed === 0;
                if (isLastChunk || !speedState || isFirstUpdate || (now - speedState.lastUIUpdate) >= 300) {
                    if (speedState) speedState.lastUIUpdate = now;
                    const { speed, remainingSeconds } = computeSpeedInfo(parsed.transferId, transferredBytes, state.fileSize);
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
        };
    }, [parseBinaryChunk, updateTransfer]);

    const handleControlMessage = useCallback(async (msg: FileTransferMessage) => {
        switch (msg.type) {
            case 'download_response': {
                const resp = msg as DownloadResponse;
                // Initialize download state
                downloadStates.current.set(resp.transfer_id, {
                    fileName: resp.file_name,
                    fileSize: resp.file_size,
                    totalChunks: resp.total_chunks,
                    receivedChunks: 0,
                    transferredBytes: 0,
                    chunks: new Array(resp.total_chunks),
                });

                updateTransfer(resp.transfer_id, { status: 'transferring', fileSize: resp.file_size });
                // Reset speed state to when actual data transfer begins
                transferSpeedState.current.set(resp.transfer_id, { startTime: Date.now(), lastCalcTime: Date.now(), lastCalcBytes: 0, lastUIUpdate: Date.now(), emaSpeed: 0 });
                break;
            }
            case 'upload_response': {
                const resp = msg as UploadResponse;
                if (resp.accepted) {
                    updateTransfer(resp.transfer_id, { status: 'transferring' });
                } else {
                    updateTransfer(resp.transfer_id, {
                        status: 'error',
                        errorMessage: resp.message || 'Upload rejected',
                    });
                    removeTransferAfterDelay(resp.transfer_id);
                }
                break;
            }
            case 'transfer_complete': {
                const complete = msg as TransferComplete;
                const state = downloadStates.current.get(complete.transfer_id);
                if (state) {
                    // Download complete — assemble and save
                    const totalSize = state.chunks.reduce((sum, c) => sum + (c ? c.length : 0), 0);
                    const assembled = new Uint8Array(totalSize);
                    let offset = 0;
                    for (const chunk of state.chunks) {
                        if (chunk) {
                            assembled.set(chunk, offset);
                            offset += chunk.length;
                        }
                    }

                    const blob = new Blob([assembled]);

                    // Try File System Access API (lets user choose save location)
                    // Falls back to default download if unsupported or user cancels
                    const saved = await saveFileWithPicker(blob, state.fileName);
                    if (!saved) {
                        triggerBrowserDownload(blob, state.fileName);
                    }

                    downloadStates.current.delete(complete.transfer_id);
                }
                updateTransfer(complete.transfer_id, {
                    status: 'completed',
                    progress: 100,
                });
                removeTransferAfterDelay(complete.transfer_id, 60000);
                break;
            }
            case 'transfer_error': {
                const error = msg as TransferError;
                downloadStates.current.delete(error.transfer_id);
                updateTransfer(error.transfer_id, {
                    status: 'error',
                    errorMessage: error.message,
                });
                removeTransferAfterDelay(error.transfer_id);
                break;
            }
        }
    }, [updateTransfer, removeTransferAfterDelay]);

    // Establish WebRTC connection via signaling, return data channel
    const ensureConnection = useCallback(async (): Promise<RTCDataChannel> => {
        // Already connected
        if (dcRef.current && dcRef.current.readyState === 'open') {
            return dcRef.current;
        }

        if (!deskId) throw new Error('No desk ID');

        return new Promise<RTCDataChannel>((resolve, reject) => {
            connectPromiseRef.current = { resolve, reject };

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
                // Request remote session
                const msg = {
                    request_id: uuidv4(),
                    signaling_type: SIGNALING_TYPE_CODE_REQUEST_REMOTE,
                    signaling_data: { connection_id: deskId },
                    to_connection_id: deskId,
                };
                ws.send(JSON.stringify(msg));
            };

            ws.onerror = (err) => {
                console.error('File transfer WS error:', err);
                reject(new Error('WebSocket connection failed'));
                connectPromiseRef.current = null;
            };

            ws.onclose = () => {
                console.log('File transfer WS closed');
            };

            ws.onmessage = async (event) => {
                try {
                    const signaling = JSON.parse(event.data);
                    const { signaling_type, signaling_data } = signaling;

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
                            console.log('File transfer data channel open');
                            if (connectPromiseRef.current) {
                                connectPromiseRef.current.resolve(dc);
                                connectPromiseRef.current = null;
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
                    console.error('File transfer signaling error:', e);
                }
            };
        });
    }, [deskId, setupDataChannelHandlers]);

    // Close WebRTC and WebSocket connections
    const closeConnection = useCallback(() => {
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

        // Add transfer to list
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
        } catch (err) {
            updateTransfer(transferId, {
                status: 'error',
                errorMessage: err instanceof Error ? err.message : 'Connection failed',
            });
            removeTransferAfterDelay(transferId);
        }
    }, [ensureConnection, updateTransfer, removeTransferAfterDelay]);

    // Upload a file
    const uploadFile = useCallback(async (targetDir: string, file: File) => {
        const transferId = uuidv4();

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
            const totalChunks = Math.ceil(file.size / chunkSize) || 1;

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

            // Wait a bit for the response, then send chunks
            // (In practice the DC message handler will update the state)
            // For now, proceed with sending chunks immediately after a short wait
            await new Promise(r => setTimeout(r, 100));

            const reader = file.stream().getReader();
            let chunkIndex = 0;
            let sentBytes = 0;
            let lastUploadUpdate = Date.now();
            let leftover = new Uint8Array(0);

            while (true) {
                // Check if this upload has been cancelled
                if (cancelledTransfers.current.has(transferId)) {
                    cancelledTransfers.current.delete(transferId);
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
                    if (cancelledTransfers.current.has(transferId)) {
                        cancelledTransfers.current.delete(transferId);
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

                    // Debug Sampling
                    if (chunkIndex % 100 === 0) {
                        const { speed } = computeSpeedInfo(transferId, sentBytes, file.size);
                        console.log(`Upload transfer ${transferId}: speed=${speed}, bufferedAmount=${dc.bufferedAmount}`);
                    }

                    // Robust backpressure: wait if browser send buffer is too full
                    if (dc.bufferedAmount > 2 * 1024 * 1024) {
                        console.warn(`Upload backpressure triggered for ${transferId}: bufferedAmount=${dc.bufferedAmount}, pausing...`);
                        while (dc.bufferedAmount > 512 * 1024) {
                            await new Promise(r => setTimeout(r, 100));
                        }
                        console.log(`Upload backpressure released for ${transferId}`);
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

            updateTransfer(transferId, {
                status: 'completed',
                progress: 100,
                transferredBytes: file.size,
            });
            removeTransferAfterDelay(transferId, 60000);

        } catch (err) {
            updateTransfer(transferId, {
                status: 'error',
                errorMessage: err instanceof Error ? err.message : 'Upload failed',
            });
            removeTransferAfterDelay(transferId);
        }
    }, [ensureConnection, buildBinaryChunk, updateTransfer, removeTransferAfterDelay]);

    // Cancel an active transfer (download or upload)
    const cancelTransfer = useCallback((transferId: string) => {
        // Send cancel message to server
        if (dcRef.current && dcRef.current.readyState === 'open') {
            const cancel: TransferCancel = {
                type: 'transfer_cancel',
                transfer_id: transferId,
            };
            dcRef.current.send(JSON.stringify(cancel));
        }

        // Mark as cancelled so the upload loop will stop
        cancelledTransfers.current.add(transferId);

        // Clean up local download state
        downloadStates.current.delete(transferId);

        // Update UI
        updateTransfer(transferId, {
            status: 'error',
            errorMessage: 'Cancelled',
        });
        removeTransferAfterDelay(transferId, 60000);
    }, [updateTransfer, removeTransferAfterDelay]);

    // Manually remove a transfer from the list
    const removeTransfer = useCallback((transferId: string) => {
        setTransfers(prev => prev.filter(t => t.transferId !== transferId));
        // Also clean up any associated state if necessary
        transferSpeedState.current.delete(transferId);
        downloadStates.current.delete(transferId);
        cancelledTransfers.current.add(transferId);
    }, []);

    return {
        transfers,
        downloadFile,
        uploadFile,
        cancelTransfer,
        removeTransfer,
        closeConnection,
    };
}
