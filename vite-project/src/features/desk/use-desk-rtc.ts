
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
    const [isRTCConnected, setIsRTCConnected] = useState(false);

    const [rtcStats, setRtcStats] = useState<RTCStatsData>({
        fps: 0, bitrate: 0, rtt: 0,
        width: 0, height: 0,
        videoCodec: '', audioCodec: '',
        packetLoss: 0, networkType: ''
    });
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
                }
            } else if (signaling_type === SIGNALING_TYPE_CODE_CANID) {
                const pc = peerConnection.current;
                if (pc) {
                    await pc.addIceCandidate(new RTCIceCandidate(signaling_data));
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
            if (pc.iceConnectionState === 'connected' || pc.iceConnectionState === 'completed') {
                setIsRTCConnected(true);
            } else if (pc.iceConnectionState === 'disconnected' || pc.iceConnectionState === 'failed') {
                setIsRTCConnected(false);
            }
        }

        pc.ontrack = (event) => {
            console.log('Received remote track', event.streams[0]);

            // Disable Jitter Buffer to achieve absolute zero playout latencies
            if (event.receiver && 'playoutDelayHint' in event.receiver) {
                try {
                    (event.receiver as any).playoutDelayHint = 0;
                } catch (e) {
                    console.warn('Failed to set playoutDelayHint', e);
                }
            }

            setRemoteStream(event.streams[0]);
        };

        // Add Transceivers
        pc.addTransceiver('video', { direction: 'sendrecv' });
        pc.addTransceiver('audio', { direction: 'sendrecv' });

        // Create Data Channels
        mouseChannel.current = pc.createDataChannel("mouse_event", { ordered: true });
        keyboardChannel.current = pc.createDataChannel("keyboard_event", { ordered: true });
        // { ordered: false, maxRetransmits: 0 } means unreliable and unordered UDP style channel for high-frequency updates
        mouseMoveChannel.current = pc.createDataChannel("mouse_move_event", { ordered: false, maxRetransmits: 0 });

        mouseChannel.current.onopen = () => console.log("Mouse channel open");
        keyboardChannel.current.onopen = () => console.log("Keyboard channel open");
        mouseMoveChannel.current.onopen = () => console.log("Mouse Move channel open");

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
        remoteStream,
        initData,
        connect,
        closeRTC,
        mouseChannel,
        keyboardChannel,
        mouseMoveChannel,
        isRTCConnected,
        rtcStats
    };
}
