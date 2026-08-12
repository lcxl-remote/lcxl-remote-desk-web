import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, renderHook } from '@testing-library/react';
import { useDeskRTC } from './use-desk-rtc';
import useDeskRtcSource from './use-desk-rtc.ts?raw';
import type { SignalingMessage } from './use-desk-signaling';
import {
    SIGNALING_TYPE_CODE_REMOTE_ACCESS_INITIALIZED,
    SIGNALING_TYPE_CODE_ANSWER,
    SIGNALING_TYPE_CODE_ICE_CANDIDATE,
} from './constants';

// Global ordering log shared by the mocked PeerConnection so a test can
// assert that `setRemoteDescription` precedes every `addIceCandidate`, and
// that no candidate of a burst was dropped.
let callLog: string[] = [];
let peerConnectionCount = 0;
let peerConnectionCloseCount = 0;
let latestPeerConnection: MockRTCPeerConnection | null = null;
let deferSetRemoteDescription = true;
let iceRestartOfferCount = 0;
let createOfferCount = 0;
let createOfferError: Error | null = null;
let setLocalDescriptionError: Error | null = null;
let offerSdp = '';
// Captured resolver for the deferred `setRemoteDescription`, so a test can
// hold the ANSWER handler mid-flight while a candidate burst arrives.
let resolveSetRemote: (() => void) | null = null;

const LIVE_OFFER_SDP = [
    'v=0',
    'a=ice-ufrag:LOCAL',
    'm=audio 9 UDP/TLS/RTP/SAVPF 111 63 9 0 8 13 110 126',
    'a=mid:1',
    'a=sendrecv',
    'a=rtpmap:111 opus/48000/2',
    'a=rtcp-fb:111 transport-cc',
    'a=fmtp:111 minptime=10;useinbandfec=1',
    'a=rtpmap:63 red/48000/2',
    'a=fmtp:63 111/111',
    '',
].join('\r\n');

class MockRTCSessionDescription {
    constructor(init: any) {
        Object.assign(this, init);
    }
}

class MockRTCIceCandidate {
    candidate?: string;
    usernameFragment?: string | null;
    constructor(init: any) {
        Object.assign(this, init);
    }
}

class MockRTCPeerConnection {
    onicecandidate: ((ev: any) => void) | null = null;
    oniceconnectionstatechange: (() => void) | null = null;
    onconnectionstatechange: (() => void) | null = null;
    onsignalingstatechange: (() => void) | null = null;
    ontrack: ((ev: any) => void) | null = null;
    iceConnectionState = 'new';
    connectionState = 'new';
    signalingState = 'stable';
    localDescription: any = null;
    remoteDescription: any = null;

    constructor(_config: any) {
        peerConnectionCount += 1;
        latestPeerConnection = this;
    }

    addTransceiver() {}
    createDataChannel() {
        return { onopen: null } as any;
    }
    async createOffer(options?: RTCOfferOptions) {
        createOfferCount += 1;
        if (options?.iceRestart) iceRestartOfferCount += 1;
        if (createOfferError) throw createOfferError;
        return { type: 'offer', sdp: offerSdp };
    }
    async setLocalDescription(desc: any) {
        if (setLocalDescriptionError) throw setLocalDescriptionError;
        this.localDescription = desc;
    }
    setRemoteDescription(desc: any) {
        callLog.push('setRemote');
        this.remoteDescription = { type: desc.type };
        if (!deferSetRemoteDescription) return Promise.resolve();
        // Deferred: stays pending until the test resolves it, modelling the
        // async gap during which a trickled candidate burst arrives.
        return new Promise<void>((resolve) => {
            resolveSetRemote = resolve;
        });
    }
    async addIceCandidate(cand: any) {
        callLog.push(`addIce:${cand.candidate}`);
    }
    close() {
        peerConnectionCloseCount += 1;
    }
}

function makeSignalingHarness() {
    const handlers = new Set<(m: SignalingMessage) => void>();
    const subscribe = (h: (m: SignalingMessage) => void) => {
        handlers.add(h);
        return () => {
            handlers.delete(h);
        };
    };
    const emit = (m: SignalingMessage) => {
        handlers.forEach((h) => h(m));
    };
    return { subscribe, emit };
}

describe('useDeskRTC inbound signaling drain', () => {
    let originalPC: any;
    let originalSDP: any;
    let originalCand: any;

    beforeEach(() => {
        vi.useFakeTimers();
        callLog = [];
        peerConnectionCount = 0;
        peerConnectionCloseCount = 0;
        latestPeerConnection = null;
        deferSetRemoteDescription = true;
        iceRestartOfferCount = 0;
        createOfferCount = 0;
        createOfferError = null;
        setLocalDescriptionError = null;
        offerSdp = LIVE_OFFER_SDP;
        resolveSetRemote = null;
        originalPC = (globalThis as any).RTCPeerConnection;
        originalSDP = (globalThis as any).RTCSessionDescription;
        originalCand = (globalThis as any).RTCIceCandidate;
        (globalThis as any).RTCPeerConnection = MockRTCPeerConnection;
        (globalThis as any).RTCSessionDescription = MockRTCSessionDescription;
        (globalThis as any).RTCIceCandidate = MockRTCIceCandidate;
    });

    afterEach(() => {
        (globalThis as any).RTCPeerConnection = originalPC;
        (globalThis as any).RTCSessionDescription = originalSDP;
        (globalThis as any).RTCIceCandidate = originalCand;
        vi.useRealTimers();
    });

    /**
     * The root-cause regression: a generation's ANSWER followed by a burst
     * of trickled candidates must apply ALL candidates, in order, and only
     * after the remote description is set — even when the candidates arrive
     * while `setRemoteDescription` is still in flight. The serialized FIFO
     * drain guarantees this; the previous single-value `lastMessage` channel
     * lost the middle of every burst.
     */
    it('applies an ANSWER then every candidate of a burst, in order', async () => {
        let offerRequestId = '';
        const sendTracked = vi.fn((opts: any) => {
            offerRequestId = opts.requestId;
            opts.onSent?.(opts.requestId);
            return { requestId: opts.requestId, disposition: 'sent' as const };
        });
        const sendMessage = vi.fn(() => 'msg-id');
        const cancelQueued = vi.fn();
        const { subscribe, emit } = makeSignalingHarness();

        const { result } = renderHook(() =>
            useDeskRTC({
                deskId: 'desk-1',
                subscribe,
                sendMessage,
                sendTracked,
                cancelQueued,
            }),
        );

        // REMOTE_ACCESS_INITIALIZED primes initData so connect() can build the PeerConnection.
        await act(async () => {
            emit({
                signaling_type: SIGNALING_TYPE_CODE_REMOTE_ACCESS_INITIALIZED,
                signaling_data: { ice_servers: [] },
            });
        });

        await act(async () => {
            await result.current.connect({ video_quality: 1 });
        });
        expect(offerRequestId).not.toBe('');

        // Feed the matching ANSWER. Its handler awaits the deferred
        // setRemoteDescription, parking the drain mid-flight.
        await act(async () => {
            emit({
                request_id: offerRequestId,
                signaling_type: SIGNALING_TYPE_CODE_ANSWER,
                signaling_data: { type: 'answer', sdp: 'a=ice-ufrag:REMOTE\r\n' },
            });
        });
        expect(callLog).toEqual(['setRemote']);

        // While the ANSWER handler is blocked, a 5-candidate burst arrives
        // in one synchronous tick. Each enqueues behind the in-flight drain.
        await act(async () => {
            for (let n = 0; n < 5; n += 1) {
                emit({
                    signaling_type: SIGNALING_TYPE_CODE_ICE_CANDIDATE,
                    signaling_data: { candidate: `c${n}` },
                });
            }
        });
        // Still nothing applied — setRemoteDescription has not resolved.
        expect(callLog).toEqual(['setRemote']);

        // Resolve the remote description; the drain resumes and processes
        // the whole queued burst in arrival order.
        await act(async () => {
            resolveSetRemote?.();
            await Promise.resolve();
            await Promise.resolve();
        });

        expect(callLog).toEqual([
            'setRemote',
            'addIce:c0',
            'addIce:c1',
            'addIce:c2',
            'addIce:c3',
            'addIce:c4',
        ]);
    });

    it('renegotiates a stable pipeline without replacing the PeerConnection', async () => {
        const sendTracked = vi.fn((opts: any) => {
            opts.onSent?.(opts.requestId);
            return { requestId: opts.requestId, disposition: 'sent' as const };
        });
        const { subscribe, emit } = makeSignalingHarness();
        const { result } = renderHook(() =>
            useDeskRTC({
                deskId: 'desk-1',
                subscribe,
                sendMessage: vi.fn(() => 'msg-id'),
                sendTracked,
                cancelQueued: vi.fn(),
            }),
        );

        await act(async () => {
            emit({
                signaling_type: SIGNALING_TYPE_CODE_REMOTE_ACCESS_INITIALIZED,
                signaling_data: { ice_servers: [] },
            });
        });
        await act(async () => {
            await result.current.connect({ video_encoder: 'H264' });
            await result.current.renegotiate({ video_encoder: 'X264' });
        });

        expect(sendTracked).toHaveBeenCalledTimes(2);
        expect(peerConnectionCount).toBe(1);
        expect(peerConnectionCloseCount).toBe(0);
    });

    it('normalizes initial and renegotiated Offers without changing RED', async () => {
        const sendTracked = vi.fn((opts: any) => {
            opts.onSent?.(opts.requestId);
            return { requestId: opts.requestId, disposition: 'sent' as const };
        });
        const { subscribe, emit } = makeSignalingHarness();
        const { result } = renderHook(() =>
            useDeskRTC({
                deskId: 'desk-1',
                subscribe,
                sendMessage: vi.fn(() => 'msg-id'),
                sendTracked,
                cancelQueued: vi.fn(),
            }),
        );

        await act(async () => {
            emit({
                signaling_type: SIGNALING_TYPE_CODE_REMOTE_ACCESS_INITIALIZED,
                signaling_data: { ice_servers: [] },
            });
        });
        await act(async () => {
            await result.current.connect({ video_encoder: 'H264' });
            await result.current.renegotiate({ video_encoder: 'X264' });
        });

        expect(createOfferCount).toBe(2);
        expect(sendTracked).toHaveBeenCalledTimes(2);
        for (const [options] of sendTracked.mock.calls) {
            expect(options.data.offer.sdp).toContain(
                'a=fmtp:111 minptime=10;useinbandfec=1;stereo=1\r\n',
            );
            expect(options.data.offer.sdp).toContain('a=fmtp:63 111/111\r\n');
        }
    });

    it('re-sends the cached normalized Offer without creating a new one', async () => {
        const sendTracked = vi.fn((opts: any) => {
            opts.onSent?.(opts.requestId);
            return { requestId: opts.requestId, disposition: 'sent' as const };
        });
        const { subscribe, emit } = makeSignalingHarness();
        const { result } = renderHook(() =>
            useDeskRTC({
                deskId: 'desk-1',
                subscribe,
                sendMessage: vi.fn(() => 'msg-id'),
                sendTracked,
                cancelQueued: vi.fn(),
            }),
        );

        await act(async () => {
            emit({
                signaling_type: SIGNALING_TYPE_CODE_REMOTE_ACCESS_INITIALIZED,
                signaling_data: { ice_servers: [] },
            });
        });
        await act(async () => {
            await result.current.connect({ video_encoder: 'H264' });
        });
        const cachedOffer = sendTracked.mock.calls[0][0].data;

        await act(async () => {
            await vi.advanceTimersByTimeAsync(5000);
        });

        expect(createOfferCount).toBe(1);
        expect(sendTracked).toHaveBeenCalledTimes(2);
        expect(sendTracked.mock.calls[1][0].data).toBe(cachedOffer);
        expect(sendTracked.mock.calls[1][0].data.offer.sdp).toContain('stereo=1');
    });

    it('normalizes an ICE-restart Offer and preserves the iceRestart option', async () => {
        const sendTracked = vi.fn((opts: any) => {
            opts.onSent?.(opts.requestId);
            return { requestId: opts.requestId, disposition: 'sent' as const };
        });
        const { subscribe, emit } = makeSignalingHarness();
        const { result } = renderHook(() =>
            useDeskRTC({
                deskId: 'desk-1',
                subscribe,
                sendMessage: vi.fn(() => 'msg-id'),
                sendTracked,
                cancelQueued: vi.fn(),
            }),
        );

        await act(async () => {
            emit({
                signaling_type: SIGNALING_TYPE_CODE_REMOTE_ACCESS_INITIALIZED,
                signaling_data: { ice_servers: [] },
            });
        });
        await act(async () => {
            await result.current.connect({ video_encoder: 'H264' });
        });
        await act(async () => {
            latestPeerConnection!.iceConnectionState = 'failed';
            latestPeerConnection!.oniceconnectionstatechange?.();
            await Promise.resolve();
            await Promise.resolve();
        });

        expect(createOfferCount).toBe(2);
        expect(iceRestartOfferCount).toBe(1);
        expect(sendTracked).toHaveBeenCalledTimes(2);
        expect(sendTracked.mock.calls[1][0].data.offer.sdp).toContain('stereo=1');
    });

    it('continues to propagate native Offer and local-description failures', async () => {
        const { subscribe, emit } = makeSignalingHarness();
        const { result } = renderHook(() =>
            useDeskRTC({
                deskId: 'desk-1',
                subscribe,
                sendMessage: vi.fn(() => 'msg-id'),
                sendTracked: vi.fn(() => ({ requestId: 'id', disposition: 'sent' as const })),
                cancelQueued: vi.fn(),
            }),
        );

        await act(async () => {
            emit({
                signaling_type: SIGNALING_TYPE_CODE_REMOTE_ACCESS_INITIALIZED,
                signaling_data: { ice_servers: [] },
            });
        });

        createOfferError = new Error('createOffer failed');
        await expect(result.current.connect({})).rejects.toThrow('createOffer failed');

        createOfferError = null;
        setLocalDescriptionError = new Error('setLocalDescription failed');
        await expect(result.current.connect({})).rejects.toThrow('setLocalDescription failed');
    });

    it('passes through an Offer without audio when stereo normalization is inapplicable', async () => {
        offerSdp = 'v=0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n';
        const sendTracked = vi.fn((opts: any) => ({
            requestId: opts.requestId,
            disposition: 'sent' as const,
        }));
        const { subscribe, emit } = makeSignalingHarness();
        const { result } = renderHook(() =>
            useDeskRTC({
                deskId: 'desk-1',
                subscribe,
                sendMessage: vi.fn(() => 'msg-id'),
                sendTracked,
                cancelQueued: vi.fn(),
            }),
        );

        await act(async () => {
            emit({
                signaling_type: SIGNALING_TYPE_CODE_REMOTE_ACCESS_INITIALIZED,
                signaling_data: { ice_servers: [] },
            });
        });
        await act(async () => {
            await result.current.connect({});
        });

        expect(sendTracked.mock.calls[0][0].data.offer.sdp).toBe(offerSdp);
    });

    it('does not ICE-restart after a settings-only renegotiation on an already connected transport', async () => {
        const requestIds: string[] = [];
        const sendTracked = vi.fn((opts: any) => {
            requestIds.push(opts.requestId);
            opts.onSent?.(opts.requestId);
            return { requestId: opts.requestId, disposition: 'sent' as const };
        });
        const { subscribe, emit } = makeSignalingHarness();
        const { result } = renderHook(() =>
            useDeskRTC({
                deskId: 'desk-1',
                subscribe,
                sendMessage: vi.fn(() => 'msg-id'),
                sendTracked,
                cancelQueued: vi.fn(),
            }),
        );

        await act(async () => {
            emit({
                signaling_type: SIGNALING_TYPE_CODE_REMOTE_ACCESS_INITIALIZED,
                signaling_data: { ice_servers: [] },
            });
        });
        await act(async () => {
            await result.current.connect({ video_encoder: 'H264' });
            await result.current.renegotiate({ video_encoder: 'X264' });
        });

        // This is the exact encoder-switch shape from the repro: SDP changes
        // while the existing ICE transport is still connected. Browsers do
        // not emit a new state-change event merely because the ANSWER landed.
        expect(latestPeerConnection).not.toBeNull();
        latestPeerConnection!.iceConnectionState = 'connected';
        deferSetRemoteDescription = false;
        await act(async () => {
            emit({
                request_id: requestIds[1],
                signaling_type: SIGNALING_TYPE_CODE_ANSWER,
                signaling_data: { type: 'answer', sdp: 'a=ice-ufrag:REMOTE-2\r\n' },
            });
            await Promise.resolve();
        });

        await act(async () => {
            await vi.advanceTimersByTimeAsync(5001);
        });

        expect(iceRestartOfferCount).toBe(0);
        expect(sendTracked).toHaveBeenCalledTimes(2);
    });
});

describe('useDeskRTC Offer creation guard', () => {
    it('allows exactly one direct pc.createOffer call inside the stereo wrapper', () => {
        const occurrences = [...useDeskRtcSource.matchAll(/\bpc\.createOffer\s*\(/g)];
        const wrapperStart = useDeskRtcSource.indexOf('async function createAndSetStereoOffer');
        const hookStart = useDeskRtcSource.indexOf('export function useDeskRTC');

        expect(occurrences).toHaveLength(1);
        expect(wrapperStart).toBeGreaterThanOrEqual(0);
        expect(occurrences[0].index).toBeGreaterThan(wrapperStart);
        expect(occurrences[0].index).toBeLessThan(hookStart);
    });
});
