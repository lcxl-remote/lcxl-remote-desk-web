
import { useEffect, useRef, useState, useCallback } from 'react';
import {
    SIGNALING_TYPE_CODE_INIT,
    SIGNALING_TYPE_CODE_OFFER,
    SIGNALING_TYPE_CODE_ANSWER,
    SIGNALING_TYPE_CODE_CANID,
    SIGNALING_TYPE_CODE_ERROR,
} from './constants';
import type { SignalingMessage } from './use-desk-signaling';

type UseDeskRTCProps = {
    deskId: string | null;
    lastMessage: SignalingMessage | null;
    sendMessage: (
        type: number,
        data: any,
        connectionId?: string,
        requestId?: string,
    ) => string;
};

export type RTCStatsData = {
    fps: number;
    bitrate: number; // kbps
    rtt: number; // ms
    width: number;
    height: number;
    videoCodec: string;
    audioCodec: string;
    packetLoss: number;
    networkType: string;
    // Frame-level diagnostics surfaced from the browser's
    // `RTCInboundRtpStreamStats`. These are the closest signal we have
    // for "what is actually arriving and decoding" — a backend-side
    // counter would give the encoder's view, but for triage of the
    // P/I-frame storm class of bugs the receiver's view is the one
    // that matters.
    framesDecoded: number;       // total decoded video frames since start
    keyFramesDecoded: number;    // I frames (the absolute counter)
    pFramesDecoded: number;      // derived = framesDecoded - keyFramesDecoded
    framesDecodedDelta: number;  // framesDecoded change over the last sample window (~1s)
    keyFramesDelta: number;      // I-frame rate (per sample window)
    pliCount: number;            // total PLI we've sent to the sender
    nackCount: number;
    firCount: number;
    pliDelta: number;            // PLI rate (per sample window)
    framesDropped: number;
    // `qpSum / framesDecoded` — lower is sharper. `null` means the
    // browser did not report `qpSum` for this codec/decoder path,
    // which is expected on Chromium with GPU-accelerated H.264 on
    // many Windows GPUs (NVDEC / QuickSync drivers don't expose QP
    // out of the hw decoder). Distinguish that from "0 frames decoded
    // yet" so the UI can render "N/A" honestly instead of a misleading
    // "0" or "-".
    avgQp: number | null;
    freezeCount: number;
    totalFreezesDurationMs: number;
    jitterMs: number;
};

export function useDeskRTC({ deskId, lastMessage, sendMessage }: UseDeskRTCProps) {
    const peerConnection = useRef<RTCPeerConnection | null>(null);
    const [remoteStream, setRemoteStream] = useState<MediaStream | null>(null);
    const [initData, setInitData] = useState<any | null>(null);
    const mouseChannel = useRef<RTCDataChannel | null>(null);
    const keyboardChannel = useRef<RTCDataChannel | null>(null);
    const mouseMoveChannel = useRef<RTCDataChannel | null>(null);
    const clipboardChannel = useRef<RTCDataChannel | null>(null); // Added clipboardChannel ref
    const fileTransferChannel = useRef<RTCDataChannel | null>(null); // Added fileTransferChannel ref
    const whiteboardChannel = useRef<RTCDataChannel | null>(null);
    const cursorSyncChannel = useRef<RTCDataChannel | null>(null);
    const [isRTCConnected, setIsRTCConnected] = useState(false);
    // Terminal ICE failure, distinct from a transient `disconnected`. ICE
    // routinely dips to `disconnected` (consent refresh, relay path changes)
    // and recovers on its own; only `failed` means negotiation gave up. The UI
    // uses this to decide whether to reopen the config dialog — a transient
    // drop must not, or the recovered video ends up behind a spurious dialog.
    const [rtcFailed, setRtcFailed] = useState(false);

    const [rtcStats, setRtcStats] = useState<RTCStatsData>({
        fps: 0, bitrate: 0, rtt: 0,
        width: 0, height: 0,
        videoCodec: '', audioCodec: '',
        packetLoss: 0, networkType: '',
        framesDecoded: 0, keyFramesDecoded: 0, pFramesDecoded: 0,
        framesDecodedDelta: 0, keyFramesDelta: 0,
        pliCount: 0, nackCount: 0, firCount: 0, pliDelta: 0,
        framesDropped: 0, avgQp: null,
        freezeCount: 0, totalFreezesDurationMs: 0,
        jitterMs: 0,
    });
    const remoteCandidatesQueue = useRef<RTCIceCandidateInit[]>([]);
    const lastBytesReceivedRef = useRef<number>(0);
    const lastStatsTimeRef = useRef<number>(0);
    const lastPacketsLostRef = useRef<number>(0);
    const lastPacketsReceivedRef = useRef<number>(0);
    // Snapshots of monotonically-increasing video counters so we can
    // derive per-sample-window deltas. We display both the absolute
    // total (e.g. "120 I frames since start") and the rate (e.g.
    // "2 I frames/sample") because an absolute number is needed for
    // long-running diagnostics while the delta tells you the *current*
    // behaviour at a glance.
    const lastFramesDecodedRef = useRef<number>(0);
    const lastKeyFramesRef = useRef<number>(0);
    const lastPliCountRef = useRef<number>(0);

    // Handle incoming signaling messages
    useEffect(() => {
        if (!lastMessage || !deskId) return;

        const { signaling_type, signaling_data } = lastMessage;

        const handleSignaling = async () => {
            // The daemon owns the WebRTC PC and
            // keeps it alive across worker swaps (UAC / lock screen /
            // session change). Browser-facing DesktopSwitching /
            // DesktopReady signals are no longer emitted, so the
            // tear-down-and-reconnect path that lived here is gone.
            if (signaling_type === SIGNALING_TYPE_CODE_INIT) {
                console.log('Received INIT', signaling_data);
                setInitData(signaling_data);

            } else if (signaling_type === SIGNALING_TYPE_CODE_ANSWER) {
                console.log('Received ANSWER');
                const pc = peerConnection.current;
                if (pc) {
                    await pc.setRemoteDescription(new RTCSessionDescription(signaling_data));
                    console.log(`[WebRTC] Remote description set successfully. Flushing ${remoteCandidatesQueue.current.length} queued candidates.`);

                    // Flush the ICE candidate queue
                    while (remoteCandidatesQueue.current.length > 0) {
                        const candidate = remoteCandidatesQueue.current.shift();
                        if (candidate) {
                            try {
                                await pc.addIceCandidate(new RTCIceCandidate(candidate));
                            } catch (e) {
                                console.warn('[WebRTC] Error adding queued ICE candidate', e);
                            }
                        }
                    }
                }
            } else if (signaling_type === SIGNALING_TYPE_CODE_CANID) {
                const pc = peerConnection.current;
                if (pc) {
                    if (pc.remoteDescription && pc.remoteDescription.type) {
                        try {
                            await pc.addIceCandidate(new RTCIceCandidate(signaling_data));
                        } catch (e) {
                            console.warn('[WebRTC] Error adding ICE candidate', e);
                        }
                    } else {
                        console.log('[WebRTC] Queuing ICE candidate because remote description is not set yet');
                        remoteCandidatesQueue.current.push(signaling_data);
                    }
                }
            }
        };

        handleSignaling().catch(console.error);

    }, [lastMessage, deskId]);

    const connect = useCallback(async (settings: any) => {
        if (!initData || !deskId) return;

        console.log('Connecting with settings', settings);

        if (peerConnection.current) {
            peerConnection.current.close();
        }

        const pc = new RTCPeerConnection({
            iceServers: initData.ice_servers || [],
        });
        peerConnection.current = pc;
        // Fresh attempt: clear any terminal failure from a previous connection.
        setRtcFailed(false);

        pc.onicecandidate = (event) => {
            if (event.candidate !== null) {
                console.log("[WebRTC] Send ICE canididate:", event.candidate);
                sendMessage(SIGNALING_TYPE_CODE_CANID, event.candidate, deskId);
            }
        };

        pc.oniceconnectionstatechange = () => {
            console.log(`[WebRTC] ICE Connection State changed to: ${pc.iceConnectionState}`);
            if (pc.iceConnectionState === 'connected' || pc.iceConnectionState === 'completed') {
                setIsRTCConnected(true);
                // A successful (re)connection clears any prior terminal failure.
                setRtcFailed(false);
            } else if (pc.iceConnectionState === 'disconnected') {
                // Transient: mark the link down but NOT failed — ICE usually
                // heals on its own, so the UI must not reopen the config dialog.
                setIsRTCConnected(false);
            } else if (pc.iceConnectionState === 'failed') {
                // Terminal: negotiation gave up. Surface it so the UI can offer
                // a retry.
                setIsRTCConnected(false);
                setRtcFailed(true);
            }
        }

        pc.onconnectionstatechange = () => {
            console.log(`[WebRTC] Connection State changed to: ${pc.connectionState}`);
        }

        pc.onsignalingstatechange = () => {
            console.log(`[WebRTC] Signaling State changed to: ${pc.signalingState}`);
        }

        pc.ontrack = (event) => {
            console.log('[WebRTC] ontrack fired!', event.track.kind, 'stream:', event.streams[0]?.id);
            console.log(`[WebRTC] Track details - kind: ${event.track.kind}, enabled: ${event.track.enabled}, muted: ${event.track.muted}, readyState: ${event.track.readyState}`);

            event.track.onmute = () => console.log(`[WebRTC] Track ${event.track.kind} muted by browser (often means no data or Safari paused it)`);
            event.track.onunmute = () => console.log(`[WebRTC] Track ${event.track.kind} unmuted`);
            event.track.onended = () => console.log(`[WebRTC] Track ${event.track.kind} ended`);

            // Disable Jitter Buffer to achieve absolute zero playout latencies
            try {
                if (event.receiver && 'playoutDelayHint' in event.receiver) {
                    // Type assertion to bypass strict missing TS checks in standard DOM lib
                    const receiverWithPlayoutDelay = event.receiver as any;
                    if (receiverWithPlayoutDelay.playoutDelayHint !== undefined) {
                        receiverWithPlayoutDelay.playoutDelayHint = 0;
                    }
                }
            } catch (e) {
                console.warn('Failed to set playoutDelayHint, likely missing in this browser (e.g. Safari)', e);
            }

            setRemoteStream(event.streams[0]);
        };

        // Add Transceivers
        pc.addTransceiver('video', { direction: 'sendrecv' });
        pc.addTransceiver('audio', { direction: 'sendrecv' });

        // Create Data Channels
        mouseChannel.current = pc.createDataChannel("mouse_event", { ordered: true });
        // keyboard events channel
        keyboardChannel.current = pc.createDataChannel('keyboard_event', {
            ordered: false,
            maxRetransmits: 0,
        });
        // clipboard events channel
        clipboardChannel.current = pc.createDataChannel('clipboard_event', {
            ordered: true,
        });
        fileTransferChannel.current = pc.createDataChannel('file_transfer_event', { ordered: true });
        // { ordered: false, maxRetransmits: 0 } means unreliable and unordered UDP style channel for high-frequency updates
        mouseMoveChannel.current = pc.createDataChannel("mouse_move_event", { ordered: false, maxRetransmits: 0 });
        whiteboardChannel.current = pc.createDataChannel("whiteboard_event", { ordered: true });
        cursorSyncChannel.current = pc.createDataChannel("cursor_sync_event", { ordered: true });

        mouseChannel.current.onopen = () => console.log("Mouse channel open");
        keyboardChannel.current.onopen = () => console.log("Keyboard channel open");
        mouseMoveChannel.current.onopen = () => console.log("Mouse Move channel open");
        clipboardChannel.current.onopen = () => console.log("Clipboard channel open"); // Added onopen for clipboardChannel
        fileTransferChannel.current.onopen = () => console.log("File Transfer channel open"); // Added onopen for fileTransferChannel
        whiteboardChannel.current.onopen = () => console.log("Whiteboard channel open");
        cursorSyncChannel.current.onopen = () => console.log("Cursor Sync channel open");

        // Create Offer
        const offer = await pc.createOffer();
        await pc.setLocalDescription(offer);

        // Send Offer immediately (Trickle ICE)
        const offerModel = {
            offer: pc.localDescription,
            desk_settings: settings,
        };
        sendMessage(SIGNALING_TYPE_CODE_OFFER, offerModel, deskId);

    }, [initData, deskId, sendMessage]);

    const closeRTC = useCallback(() => {
        if (peerConnection.current) {
            peerConnection.current.close();
            peerConnection.current = null;
        }
        setIsRTCConnected(false);
        setRemoteStream(null);
    }, []);

    // RTCPeerConnection Stats Monitor
    useEffect(() => {
        if (!isRTCConnected || !peerConnection.current) return;

        const interval = setInterval(async () => {
            if (!peerConnection.current) return;
            try {
                const stats = await peerConnection.current.getStats();
                let currentFps = 0;
                let currentBitrate = 0;
                let currentRtt = 0;
                let currentWidth = 0;
                let currentHeight = 0;
                let currentVideoCodec = '';
                let currentAudioCodec = '';
                let currentPacketLoss = 0;
                let currentNetworkType = '';

                let videoCodecId = '';
                let audioCodecId = '';
                let localCandidateId = '';

                // Frame-level counters latched from the inbound video
                // report. Defaulted to the previous sample's values so
                // a brief absence of the report (e.g. mid-renegotiation)
                // doesn't reset the displayed totals to 0.
                let framesDecoded = 0;
                let keyFramesDecoded = 0;
                let pliCount = 0;
                let nackCount = 0;
                let firCount = 0;
                let framesDropped = 0;
                // `undefined` here means the browser did NOT include
                // `qpSum` in its inbound-rtp report (codec/decoder path
                // that doesn't expose it). Tracked separately from
                // "qpSum present and equal to 0" so the UI can render
                // "N/A" only when truly unreported.
                let qpSum: number | undefined = undefined;
                let freezeCount = 0;
                let totalFreezesDurationMs = 0;
                let jitterMs = 0;

                stats.forEach(report => {
                    if (report.type === 'inbound-rtp' && report.kind === 'video') {
                        if (report.framesPerSecond !== undefined) {
                            currentFps = report.framesPerSecond;
                        }
                        if (report.frameWidth !== undefined) currentWidth = report.frameWidth;
                        if (report.frameHeight !== undefined) currentHeight = report.frameHeight;
                        if (report.codecId) videoCodecId = report.codecId;

                        // Frame-level counters: these are monotonically
                        // increasing in browsers that implement them.
                        // Spec: https://www.w3.org/TR/webrtc-stats/#dom-rtcinboundrtpstreamstats
                        framesDecoded = report.framesDecoded ?? 0;
                        keyFramesDecoded = report.keyFramesDecoded ?? 0;
                        pliCount = report.pliCount ?? 0;
                        nackCount = report.nackCount ?? 0;
                        firCount = report.firCount ?? 0;
                        framesDropped = report.framesDropped ?? 0;
                        qpSum = report.qpSum;
                        freezeCount = report.freezeCount ?? 0;
                        totalFreezesDurationMs = Math.round((report.totalFreezesDuration ?? 0) * 1000);
                        jitterMs = Math.round((report.jitter ?? 0) * 1000);

                        const bytes = report.bytesReceived;
                        const now = report.timestamp;
                        if (lastBytesReceivedRef.current && lastStatsTimeRef.current) {
                            const bytesDiff = bytes - lastBytesReceivedRef.current;
                            const timeDiff = now - lastStatsTimeRef.current;
                            if (timeDiff > 0) {
                                // kbps (8 bits / 1000)
                                currentBitrate = Math.round((bytesDiff * 8) / timeDiff);
                            }
                        }
                        lastBytesReceivedRef.current = bytes;
                        lastStatsTimeRef.current = now;

                        // Packet loss calculation
                        const packetsLost = report.packetsLost || 0;
                        const packetsReceived = report.packetsReceived || 0;
                        if (lastPacketsLostRef.current !== undefined && lastPacketsReceivedRef.current !== undefined) {
                            const lostDiff = packetsLost - lastPacketsLostRef.current;
                            const recvDiff = packetsReceived - lastPacketsReceivedRef.current;
                            if (lostDiff + recvDiff > 0) {
                                currentPacketLoss = Number(((lostDiff / (lostDiff + recvDiff)) * 100).toFixed(2));
                            }
                        }
                        lastPacketsLostRef.current = packetsLost;
                        lastPacketsReceivedRef.current = packetsReceived;
                    }

                    if (report.type === 'inbound-rtp' && report.kind === 'audio') {
                        if (report.codecId) audioCodecId = report.codecId;
                    }

                    if (report.type === 'candidate-pair' && report.state === 'succeeded') {
                        if (report.localCandidateId) {
                            localCandidateId = report.localCandidateId;
                        }
                        if (report.currentRoundTripTime !== undefined) {
                            currentRtt = Math.round(report.currentRoundTripTime * 1000);
                        } else if (report.roundTripTime !== undefined) {
                            currentRtt = Math.round(report.roundTripTime * 1000);
                        }
                    }
                });

                if (videoCodecId || audioCodecId || localCandidateId) {
                    stats.forEach(report => {
                        if (report.type === 'codec') {
                            if (report.id === videoCodecId) {
                                currentVideoCodec = report.mimeType?.replace('video/', '') || '';
                            }
                            if (report.id === audioCodecId) {
                                currentAudioCodec = report.mimeType?.replace('audio/', '') || '';
                            }
                        }
                        if (report.type === 'local-candidate' && report.id === localCandidateId) {
                            currentNetworkType = report.candidateType || '';
                        }
                    });
                }

                // Derive per-sample-window deltas. First sample after
                // (re)connect will read 0 deltas because the refs are
                // still at their initial 0 — that's the correct
                // behaviour: we don't have a baseline to subtract.
                const framesDecodedDelta = Math.max(0, framesDecoded - lastFramesDecodedRef.current);
                const keyFramesDelta = Math.max(0, keyFramesDecoded - lastKeyFramesRef.current);
                const pliDelta = Math.max(0, pliCount - lastPliCountRef.current);
                lastFramesDecodedRef.current = framesDecoded;
                lastKeyFramesRef.current = keyFramesDecoded;
                lastPliCountRef.current = pliCount;
                const pFramesDecoded = Math.max(0, framesDecoded - keyFramesDecoded);
                // null preserves the "browser didn't report it" signal
                // even after frames have been decoded. We only round
                // to an integer when both sides of the ratio are
                // meaningful.
                const avgQp = qpSum !== undefined && framesDecoded > 0
                    ? Math.round(qpSum / framesDecoded)
                    : null;

                setRtcStats(prev => ({
                    ...prev,
                    fps: currentFps || prev.fps,
                    bitrate: currentBitrate,
                    rtt: currentRtt || prev.rtt,
                    width: currentWidth || prev.width,
                    height: currentHeight || prev.height,
                    videoCodec: currentVideoCodec || prev.videoCodec,
                    audioCodec: currentAudioCodec || prev.audioCodec,
                    packetLoss: currentPacketLoss || prev.packetLoss,
                    networkType: currentNetworkType || prev.networkType,
                    framesDecoded,
                    keyFramesDecoded,
                    pFramesDecoded,
                    framesDecodedDelta,
                    keyFramesDelta,
                    pliCount,
                    nackCount,
                    firCount,
                    pliDelta,
                    framesDropped,
                    avgQp,
                    freezeCount,
                    totalFreezesDurationMs,
                    jitterMs,
                }));

            } catch (err) {
                console.warn("Failed to get RTC stats", err);
            }
        }, 1000);

        return () => clearInterval(interval);
    }, [isRTCConnected]);

    // Ensure RTC connection is closed when the hook is unmounted
    useEffect(() => {
        return () => {
            if (peerConnection.current) {
                console.log("[WebRTC] Hook unmounting, closing peer connection");
                peerConnection.current.close();
                peerConnection.current = null;
            }
            setIsRTCConnected(false);
            setRemoteStream(null);
        };
    }, []);

    return {
        peerConnection,
        remoteStream,
        initData,
        connect,
        closeRTC,
        mouseChannel,
        keyboardChannel,
        mouseMoveChannel,
        clipboardChannel,
        fileTransferChannel,
        whiteboardChannel,
        cursorSyncChannel,
        isRTCConnected,
        rtcFailed,
        rtcStats
    };
}
