
import { useEffect, useRef, useState, useCallback } from 'react';
import {
    SIGNALING_TYPE_CODE_INIT,
    SIGNALING_TYPE_CODE_OFFER,
    SIGNALING_TYPE_CODE_ANSWER,
    SIGNALING_TYPE_CODE_CANID,
    SIGNALING_TYPE_CODE_ERROR
} from './constants';
import type { SignalingMessage } from './use-desk-signaling';

type UseDeskRTCProps = {
    deskId: string | null;
    lastMessage: SignalingMessage | null;
    sendMessage: (type: number, data: any, sessionId?: string) => void;
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
    const [isRTCConnected, setIsRTCConnected] = useState(false);

    const [rtcStats, setRtcStats] = useState<RTCStatsData>({
        fps: 0, bitrate: 0, rtt: 0,
        width: 0, height: 0,
        videoCodec: '', audioCodec: '',
        packetLoss: 0, networkType: ''
    });
    const remoteCandidatesQueue = useRef<RTCIceCandidateInit[]>([]);
    const lastBytesReceivedRef = useRef<number>(0);
    const lastStatsTimeRef = useRef<number>(0);
    const lastPacketsLostRef = useRef<number>(0);
    const lastPacketsReceivedRef = useRef<number>(0);

    // Handle incoming signaling messages
    useEffect(() => {
        if (!lastMessage || !deskId) return;

        const { signaling_type, signaling_data } = lastMessage;

        const handleSignaling = async () => {
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

        pc.onicecandidate = (event) => {
            if (event.candidate !== null) {
                sendMessage(SIGNALING_TYPE_CODE_CANID, event.candidate, deskId);
            }
        };

        pc.oniceconnectionstatechange = () => {
            console.log(`[WebRTC] ICE Connection State changed to: ${pc.iceConnectionState}`);
            if (pc.iceConnectionState === 'connected' || pc.iceConnectionState === 'completed') {
                setIsRTCConnected(true);
            } else if (pc.iceConnectionState === 'disconnected' || pc.iceConnectionState === 'failed') {
                setIsRTCConnected(false);
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

        mouseChannel.current.onopen = () => console.log("Mouse channel open");
        keyboardChannel.current.onopen = () => console.log("Keyboard channel open");
        mouseMoveChannel.current.onopen = () => console.log("Mouse Move channel open");
        clipboardChannel.current.onopen = () => console.log("Clipboard channel open"); // Added onopen for clipboardChannel
        fileTransferChannel.current.onopen = () => console.log("File Transfer channel open"); // Added onopen for fileTransferChannel
        whiteboardChannel.current.onopen = () => console.log("Whiteboard channel open");

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

                stats.forEach(report => {
                    if (report.type === 'inbound-rtp' && report.kind === 'video') {
                        if (report.framesPerSecond !== undefined) {
                            currentFps = report.framesPerSecond;
                        }
                        if (report.frameWidth !== undefined) currentWidth = report.frameWidth;
                        if (report.frameHeight !== undefined) currentHeight = report.frameHeight;
                        if (report.codecId) videoCodecId = report.codecId;

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
                    networkType: currentNetworkType || prev.networkType
                }));

            } catch (err) {
                console.warn("Failed to get RTC stats", err);
            }
        }, 1000);

        return () => clearInterval(interval);
    }, [isRTCConnected]);

    return {
        peerConnection,
        remoteStream,
        initData,
        connect,
        closeRTC, // Kept closeRTC as it was not explicitly removed
        mouseChannel,
        keyboardChannel,
        mouseMoveChannel,
        clipboardChannel, // Exposed clipboardChannel
        fileTransferChannel, // Exposed fileTransferChannel
        whiteboardChannel,
        isRTCConnected,
        rtcStats
    };
}
