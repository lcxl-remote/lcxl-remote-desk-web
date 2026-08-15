import { useRef, useState, useCallback, useEffect } from 'react';

type UseDeskMicrophoneProps = {
    peerConnection: React.RefObject<RTCPeerConnection | null>;
    isConnected: boolean;
};

export type DeskMicrophoneError =
    | 'noConnection'
    | 'notConnected'
    | 'secureContextRequired'
    | 'permissionDenied'
    | 'noAudioDevice'
    | 'deviceUnavailable'
    | 'failed'
    | 'remotePlaybackFailed';

export const microphoneErrorTranslationKeys: Record<DeskMicrophoneError, string> = {
    noConnection: 'pages.desk.microphoneErrors.noConnection',
    notConnected: 'pages.desk.microphoneErrors.notConnected',
    secureContextRequired: 'pages.desk.microphoneErrors.secureContextRequired',
    permissionDenied: 'pages.desk.microphoneErrors.permissionDenied',
    noAudioDevice: 'pages.desk.microphoneErrors.noAudioDevice',
    deviceUnavailable: 'pages.desk.microphoneErrors.deviceUnavailable',
    failed: 'pages.desk.microphoneErrors.failed',
    remotePlaybackFailed: 'pages.desk.microphoneErrors.remotePlaybackFailed',
};

function classifyMicrophoneError(error: unknown): DeskMicrophoneError {
    const name = error instanceof DOMException
        ? error.name
        : typeof error === 'object' && error !== null && 'name' in error
            ? String(error.name)
            : '';
    switch (name) {
        case 'NotAllowedError':
        case 'PermissionDeniedError':
            return 'permissionDenied';
        case 'NotFoundError':
        case 'DevicesNotFoundError':
            return 'noAudioDevice';
        case 'NotReadableError':
        case 'TrackStartError':
            return 'deviceUnavailable';
        case 'SecurityError':
            return 'secureContextRequired';
        default:
            return 'failed';
    }
}

export function useDeskMicrophone({ peerConnection, isConnected }: UseDeskMicrophoneProps) {
    const [isMicActive, setIsMicActive] = useState(false);
    const [isMicMuted, setIsMicMuted] = useState(false);
    const [micError, setMicError] = useState<DeskMicrophoneError | null>(null);
    const localStreamRef = useRef<MediaStream | null>(null);
    const senderRef = useRef<RTCRtpSender | null>(null);

    const attachTrack = useCallback(async (
        pc: RTCPeerConnection,
        track: MediaStreamTrack,
        stream: MediaStream,
    ) => {
        const audioTransceiver = pc.getTransceivers().find(
            transceiver => transceiver.receiver.track?.kind === 'audio',
        );
        if (audioTransceiver) {
            await audioTransceiver.sender.replaceTrack(track);
            senderRef.current = audioTransceiver.sender;
            return;
        }
        senderRef.current = pc.addTrack(track, stream);
    }, []);

    const startMicrophone = useCallback(async () => {
        console.log('[Mic] startMicrophone called, isConnected:', isConnected, 'pc:', !!peerConnection.current);
        const pc = peerConnection.current;
        if (!pc) {
            console.warn('[Mic] No peer connection available');
            setMicError('noConnection');
            return;
        }
        if (!isConnected) {
            console.warn('[Mic] Not connected');
            setMicError('notConnected');
            return;
        }

        // Check if mediaDevices API is available (requires secure context)
        if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
            console.error('[Mic] navigator.mediaDevices not available. Requires HTTPS or localhost.');
            setMicError('secureContextRequired');
            return;
        }

        try {
            setMicError(null);
            const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
            localStreamRef.current = stream;

            const audioTrack = stream.getAudioTracks()[0];
            if (!audioTrack) {
                console.error('[Mic] No audio track obtained from getUserMedia');
                setMicError('noAudioDevice');
                return;
            }

            const activePc = peerConnection.current;
            if (!activePc || !isConnected) {
                stream.getTracks().forEach(track => track.stop());
                localStreamRef.current = null;
                setMicError('notConnected');
                return;
            }

            // The PeerConnection may have been replaced while getUserMedia was
            // awaiting browser permission. Always attach to the current PC.
            const transceivers = activePc.getTransceivers();
            console.log('[Mic] Available transceivers:', transceivers.map(t => ({
                mid: t.mid,
                direction: t.direction,
                senderTrack: t.sender.track?.kind,
                receiverTrack: t.receiver.track?.kind,
            })));
            await attachTrack(activePc, audioTrack, stream);

            setIsMicActive(true);
            setIsMicMuted(false);
            console.log('[Mic] Microphone started successfully');
        } catch (err: unknown) {
            console.error('[Mic] Failed to start microphone:', err);
            setMicError(classifyMicrophoneError(err));
        }
    }, [attachTrack, peerConnection, isConnected]);

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

    const forceError = useCallback((_errorMsg: string) => {
        stopMicrophone(); // Clean up if running
        setMicError('remotePlaybackFailed');
    }, [stopMicrophone]);

    useEffect(() => {
        const stream = localStreamRef.current;
        const track = stream?.getAudioTracks()[0];
        if (!isMicActive || !stream || !track) return;

        if (!isConnected || !peerConnection.current) {
            // Preserve the user's microphone intent while preventing local audio
            // capture from flowing into a closed sender during PC replacement.
            track.enabled = false;
            senderRef.current = null;
            return;
        }

        let cancelled = false;
        const pc = peerConnection.current;
        void attachTrack(pc, track, stream)
            .then(() => {
                if (cancelled || peerConnection.current !== pc) return;
                track.enabled = !isMicMuted;
                setMicError(null);
            })
            .catch((error: unknown) => {
                if (cancelled) return;
                console.error('[Mic] Failed to attach microphone to replacement connection:', error);
                stopMicrophone();
                setMicError(classifyMicrophoneError(error));
            });
        return () => {
            cancelled = true;
        };
    }, [attachTrack, isConnected, isMicActive, isMicMuted, peerConnection, stopMicrophone]);

    useEffect(() => () => {
        localStreamRef.current?.getTracks().forEach(track => track.stop());
        localStreamRef.current = null;
        senderRef.current = null;
    }, []);

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
