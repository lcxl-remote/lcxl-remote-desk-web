import { act, renderHook, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { useDeskMicrophone } from './use-desk-microphone';

function makePeerConnection() {
    const replaceTrack = vi.fn(async () => {});
    const sender = { replaceTrack } as unknown as RTCRtpSender;
    const pc = {
        getTransceivers: () => [{
            receiver: { track: { kind: 'audio' } },
            sender,
        }],
        addTrack: vi.fn(),
    } as unknown as RTCPeerConnection;
    return { pc, replaceTrack };
}

describe('useDeskMicrophone replacement PeerConnection handoff', () => {
    const originalMediaDevices = Object.getOwnPropertyDescriptor(navigator, 'mediaDevices');

    afterEach(() => {
        vi.restoreAllMocks();
        if (originalMediaDevices) {
            Object.defineProperty(navigator, 'mediaDevices', originalMediaDevices);
        } else {
            delete (navigator as unknown as { mediaDevices?: MediaDevices }).mediaDevices;
        }
    });

    it('pauses the local track while disconnected and attaches it to the replacement PC', async () => {
        const track = {
            kind: 'audio',
            enabled: true,
            stop: vi.fn(),
        } as unknown as MediaStreamTrack;
        const stream = {
            getAudioTracks: () => [track],
            getTracks: () => [track],
        } as unknown as MediaStream;
        Object.defineProperty(navigator, 'mediaDevices', {
            configurable: true,
            value: { getUserMedia: vi.fn(async () => stream) },
        });

        const first = makePeerConnection();
        const second = makePeerConnection();
        const peerConnection = { current: first.pc };
        const { result, rerender, unmount } = renderHook(
            ({ connected }) => useDeskMicrophone({ peerConnection, isConnected: connected }),
            { initialProps: { connected: true } },
        );

        await act(async () => {
            await result.current.toggleMicrophone();
        });
        expect(result.current.isMicActive).toBe(true);
        expect(first.replaceTrack).toHaveBeenCalledWith(track);

        rerender({ connected: false });
        expect(track.enabled).toBe(false);
        expect(track.stop).not.toHaveBeenCalled();

        peerConnection.current = second.pc;
        rerender({ connected: true });
        await waitFor(() => expect(second.replaceTrack).toHaveBeenCalledWith(track));
        expect(track.enabled).toBe(true);
        expect(result.current.isMicActive).toBe(true);

        unmount();
        expect(track.stop).toHaveBeenCalledTimes(1);
    });

    it('returns a translatable error code when the page is not a secure context', async () => {
        delete (navigator as unknown as { mediaDevices?: MediaDevices }).mediaDevices;
        const peerConnection = { current: makePeerConnection().pc };
        const { result } = renderHook(() => useDeskMicrophone({
            peerConnection,
            isConnected: true,
        }));

        await act(async () => {
            await result.current.toggleMicrophone();
        });

        expect(result.current.micError).toBe('secureContextRequired');
    });
});
