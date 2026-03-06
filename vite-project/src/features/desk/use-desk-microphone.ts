import { useRef, useState, useCallback } from 'react';

type UseDeskMicrophoneProps = {
    peerConnection: React.RefObject<RTCPeerConnection | null>;
    isConnected: boolean;
};

export function useDeskMicrophone({ peerConnection, isConnected }: UseDeskMicrophoneProps) {
    const [isMicActive, setIsMicActive] = useState(false);
    const [isMicMuted, setIsMicMuted] = useState(false);
    const [micError, setMicError] = useState<string | null>(null);
    const localStreamRef = useRef<MediaStream | null>(null);
    const senderRef = useRef<RTCRtpSender | null>(null);

    const startMicrophone = useCallback(async () => {
        console.log('[Mic] startMicrophone called, isConnected:', isConnected, 'pc:', !!peerConnection.current);
        const pc = peerConnection.current;
        if (!pc) {
            console.warn('[Mic] No peer connection available');
            setMicError('No connection');
            return;
        }
        if (!isConnected) {
            console.warn('[Mic] Not connected');
            setMicError('Not connected');
            return;
        }

        // Check if mediaDevices API is available (requires secure context)
        if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
            console.error('[Mic] navigator.mediaDevices not available. Requires HTTPS or localhost.');
            setMicError('Microphone requires HTTPS');
            return;
        }

        try {
            setMicError(null);
            const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
            localStreamRef.current = stream;

            const audioTrack = stream.getAudioTracks()[0];
            if (!audioTrack) {
                console.error('[Mic] No audio track obtained from getUserMedia');
                setMicError('No audio device');
                return;
            }

            // Find the existing audio transceiver (direction: sendrecv) and replace its sender track
            const transceivers = pc.getTransceivers();
            console.log('[Mic] Available transceivers:', transceivers.map(t => ({
                mid: t.mid,
                direction: t.direction,
                senderTrack: t.sender.track?.kind,
                receiverTrack: t.receiver.track?.kind,
            })));

            const audioTransceiver = transceivers.find(
                t => t.receiver.track?.kind === 'audio'
            );

            if (audioTransceiver) {
                await audioTransceiver.sender.replaceTrack(audioTrack);
                senderRef.current = audioTransceiver.sender;
                console.log('[Mic] Replaced track on existing audio transceiver');
            } else {
                // Fallback: add track directly
                const sender = pc.addTrack(audioTrack, stream);
                senderRef.current = sender;
                console.log('[Mic] Added new audio track via addTrack');
            }

            setIsMicActive(true);
            setIsMicMuted(false);
            console.log('[Mic] Microphone started successfully');
        } catch (err: any) {
            console.error('[Mic] Failed to start microphone:', err);
            setMicError(err?.message || 'Microphone failed');
        }
    }, [peerConnection, isConnected]);

    const stopMicrophone = useCallback(() => {
        // Stop all tracks in the local stream
        if (localStreamRef.current) {
            localStreamRef.current.getTracks().forEach(track => track.stop());
            localStreamRef.current = null;
        }

        // Remove the sender's track
        if (senderRef.current) {
            senderRef.current.replaceTrack(null).catch(() => { });
            senderRef.current = null;
        }

        setIsMicActive(false);
        setIsMicMuted(false);
        setMicError(null);
        console.log('[Mic] Microphone stopped');
    }, []);

    const toggleMicrophone = useCallback(async () => {
        console.log('[Mic] toggleMicrophone called, isMicActive:', isMicActive);
        if (isMicActive) {
            stopMicrophone();
        } else {
            await startMicrophone();
        }
    }, [isMicActive, startMicrophone, stopMicrophone]);

    const toggleMicMute = useCallback(() => {
        if (!localStreamRef.current) return;
        const track = localStreamRef.current.getAudioTracks()[0];
        if (track) {
            track.enabled = !track.enabled;
            setIsMicMuted(!track.enabled);
        }
    }, []);

    const forceError = useCallback((errorMsg: string) => {
        stopMicrophone(); // Clean up if running
        setMicError(errorMsg);
    }, [stopMicrophone]);

    return {
        isMicActive,
        isMicMuted,
        micError,
        toggleMicrophone,
        toggleMicMute,
        stopMicrophone,
        forceError,
    };
}
