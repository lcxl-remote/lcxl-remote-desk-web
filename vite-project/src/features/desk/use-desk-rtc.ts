
import { useEffect, useRef, useState, useCallback } from 'react';
import { v4 } from 'uuid';
import {
    SIGNALING_TYPE_CODE_REMOTE_ACCESS_INITIALIZED,
    SIGNALING_TYPE_CODE_OFFER,
    SIGNALING_TYPE_CODE_ANSWER,
    SIGNALING_TYPE_CODE_ICE_CANDIDATE,
} from './constants';
import type {
    SignalingMessage,
    SignalingSubscriber,
    SendTrackedOptions,
    SendTrackedResult,
} from './use-desk-signaling';
import {
    createIceRetryCoordinator,
    type IceRetryCoordinator,
} from './ice-retry-coordinator';
import { normalizeOpusStereoSdp } from './opus-sdp';

// Auto-retry tuning. With lossless candidate delivery in place a healthy
// attempt completes its ICE checks in well under a couple of seconds on a
// wired LAN, so these windows are deliberately wide: a retry should fire
// only when checking genuinely stalls (a real network fault), never while
// legitimate gathering/checking is still in progress. The `checking` stall
// window uses capped exponential backoff (5s -> 10s -> 15s -> 15s ...) so a
// weak network gets progressively more patience without churny restarts,
// and the budget is small because — the message-loss root cause aside —
// deep retrying rarely helps.
const ICE_ANSWER_TIMEOUT_MS = 5000;
const ICE_STALL_BASE_MS = 5000;
const ICE_STALL_MAX_MS = 15000;
const MAX_ICE_RETRY = 4;

async function createAndSetStereoOffer(
    pc: RTCPeerConnection,
    options?: RTCOfferOptions,
): Promise<void> {
    const offer = await pc.createOffer(options);
    const normalizedSdp = normalizeOpusStereoSdp(offer.sdp);
    const normalizedOffer = normalizedSdp === offer.sdp
        ? offer
        : { ...offer, sdp: normalizedSdp };
    await pc.setLocalDescription(normalizedOffer);
}

/** Pull the `ice-ufrag` out of an SDP blob so trickled candidates can be
 *  matched to the generation they belong to. */
function parseIceUfrag(sdp: string | undefined | null): string | null {
    if (!sdp) return null;
    const m = sdp.match(/^a=ice-ufrag:(.+)$/m);
    return m ? m[1].trim() : null;
}

/** Stable signaling-queue dedup key for this desk's OFFER, so a superseded
 *  OFFER queued while offline is replaced rather than piling up. */
function offerReplaceKey(id: string | null): string {
    return `offer:${id ?? ''}`;
}

type UseDeskRTCProps = {
    deskId: string | null;
    subscribe: (handler: SignalingSubscriber) => () => void;
    sendMessage: (
        type: number,
        data: any,
        connectionId?: string,
        requestId?: string,
    ) => string;
    sendTracked: (opts: SendTrackedOptions) => SendTrackedResult;
    cancelQueued: (replaceKey: string) => void;
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

export function useDeskRTC({ deskId, subscribe, sendMessage, sendTracked, cancelQueued }: UseDeskRTCProps) {
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

    // Mutable handles the ICE-retry coordinator's callbacks read at fire
    // time, so the coordinator can be created once yet always see the
    // current deskId / signaling fns / cached OFFER. Refreshed every render
    // below.
    const rtcDeps = useRef({
        deskId,
        sendTracked,
        cancelQueued,
        settings: undefined as any,
        cachedOfferModel: undefined as any,
    });
    rtcDeps.current.deskId = deskId;
    rtcDeps.current.sendTracked = sendTracked;
    rtcDeps.current.cancelQueued = cancelQueued;

    // Self-healing negotiation. Created once; its callbacks resend the
    // cached OFFER (signaling loss) or issue a fresh `iceRestart` OFFER
    // (ICE stall), and drive the connected/failed UI state.
    const coordinatorRef = useRef<IceRetryCoordinator | null>(null);
    if (!coordinatorRef.current) {
        coordinatorRef.current = createIceRetryCoordinator({
            resendCachedOffer: (requestId, onSent) => {
                const d = rtcDeps.current;
                d.sendTracked({
                    type: SIGNALING_TYPE_CODE_OFFER,
                    data: d.cachedOfferModel,
                    toConnectionId: d.deskId ?? undefined,
                    requestId,
                    replaceKey: offerReplaceKey(d.deskId),
                    onSent,
                });
            },
            sendIceRestartOffer: async (requestId, onSent) => {
                const pc = peerConnection.current;
                if (!pc) throw new Error('No peer connection for ICE restart');
                await createAndSetStereoOffer(pc, { iceRestart: true });
                const d = rtcDeps.current;
                const offerModel = {
                    offer: pc.localDescription,
                    desk_settings: d.settings,
                };
                d.cachedOfferModel = offerModel;
                d.sendTracked({
                    type: SIGNALING_TYPE_CODE_OFFER,
                    data: offerModel,
                    toConnectionId: d.deskId ?? undefined,
                    requestId,
                    replaceKey: offerReplaceKey(d.deskId),
                    onSent,
                });
            },
            onConnected: () => {
                setIsRTCConnected(true);
                setRtcFailed(false);
            },
            onFailed: () => {
                setIsRTCConnected(false);
                setRtcFailed(true);
            },
            genRequestId: () => v4(),
            config: {
                answerTimeoutMs: ICE_ANSWER_TIMEOUT_MS,
                iceStallBaseMs: ICE_STALL_BASE_MS,
                iceStallMaxMs: ICE_STALL_MAX_MS,
                maxRetry: MAX_ICE_RETRY,
            },
        });
    }

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

    // Inbound signaling is buffered in a ref FIFO and drained by a single
    // serialized async loop, so a burst (trickled ICE candidates arriving
    // within one tick) is processed in arrival order with none dropped.
    // Routing this stream through a single `lastMessage` state would let
    // React coalesce rapid updates down to the burst's first and last
    // value — silently dropping the middle, which on a LAN is exactly
    // where the only routable host candidate tends to land.
    const inboundQueueRef = useRef<SignalingMessage[]>([]);
    const drainingRef = useRef(false);

    // The former effect body, parameterized by the message instead of a
    // single `lastMessage`. Stable identity (reads everything via refs)
    // so the subscription below never needs to re-register mid-stream.
    const handleSignalingMessage = useCallback(async (message: SignalingMessage) => {
        if (!rtcDeps.current.deskId) return;

        const { signaling_type, signaling_data } = message;

        // The daemon owns the WebRTC PC and
        // keeps it alive across worker swaps (UAC / lock screen /
        // session change). Browser-facing DesktopSwitching /
        // DesktopReady signals are no longer emitted, so the
        // tear-down-and-reconnect path that lived here is gone.
        if (signaling_type === SIGNALING_TYPE_CODE_REMOTE_ACCESS_INITIALIZED) {
            console.log('Received REMOTE_ACCESS_INITIALIZED', signaling_data);
            setInitData(signaling_data);

        } else if (
            signaling_type === SIGNALING_TYPE_CODE_OFFER
            && message.response_state
            && message.request_id
        ) {
            // A media preflight/negotiation rejection is a terminal response
            // to this OFFER, not lost signaling. Stop the ANSWER watchdog so it
            // does not resend the same incompatible encoder configuration.
            coordinatorRef.current?.onOfferRejected(message.request_id);
        } else if (signaling_type === SIGNALING_TYPE_CODE_ANSWER) {
            console.log('Received ANSWER');
            const coordinator = coordinatorRef.current;
            // Drop a stale ANSWER from a superseded OFFER generation
            // (an in-flight initial OFFER whose ANSWER arrives after a
            // retry already rolled the generation forward).
            if (
                coordinator &&
                message.request_id &&
                !coordinator.shouldAcceptAnswer(message.request_id)
            ) {
                console.warn('[WebRTC] Dropping stale ANSWER for request', message.request_id);
                return;
            }
            const pc = peerConnection.current;
            if (pc) {
                await pc.setRemoteDescription(new RTCSessionDescription(signaling_data));
                const ufrag = parseIceUfrag(signaling_data?.sdp);
                coordinator?.onAnswerApplied(ufrag);
                // A settings-only renegotiation reuses the already-connected
                // ICE transport. In that case applying the new ANSWER does not
                // emit another `iceconnectionstatechange`, so leaving the
                // coordinator in `checking` would fire its stall watchdog five
                // seconds later and start an unnecessary ICE restart. Reconcile
                // the current state synchronously after every ANSWER; initial
                // negotiation still reports `new`/`checking`, while a stable
                // transport immediately clears the watchdog as connected.
                coordinator?.onIceStateChange(pc.iceConnectionState);
                console.log(`[WebRTC] Remote description set (ufrag=${ufrag}). Flushing ${remoteCandidatesQueue.current.length} queued candidates.`);

                // Flush queued candidates, keeping only those that belong
                // to the current generation (matching ufrag); anything
                // from a superseded generation is dropped.
                const queued = remoteCandidatesQueue.current;
                remoteCandidatesQueue.current = [];
                for (const candidate of queued) {
                    const disposition = coordinator
                        ? coordinator.classifyCandidate(candidate.usernameFragment)
                        : 'apply';
                    if (disposition !== 'apply') continue;
                    try {
                        await pc.addIceCandidate(new RTCIceCandidate(candidate));
                    } catch (e) {
                        console.warn('[WebRTC] Error adding queued ICE candidate', e);
                    }
                }
            }
        } else if (signaling_type === SIGNALING_TYPE_CODE_ICE_CANDIDATE) {
            const pc = peerConnection.current;
            if (pc) {
                const coordinator = coordinatorRef.current;
                const candidate = signaling_data as RTCIceCandidateInit;
                const disposition = coordinator
                    ? coordinator.classifyCandidate(candidate.usernameFragment)
                    : (pc.remoteDescription?.type ? 'apply' : 'queue');
                if (disposition === 'reject') {
                    console.log('[WebRTC] Dropping stale ICE candidate (ufrag mismatch)');
                } else if (disposition === 'queue' || !pc.remoteDescription?.type) {
                    console.log('[WebRTC] Queuing ICE candidate until the matching ANSWER is applied');
                    remoteCandidatesQueue.current.push(candidate);
                } else {
                    try {
                        await pc.addIceCandidate(new RTCIceCandidate(candidate));
                    } catch (e) {
                        console.warn('[WebRTC] Error adding ICE candidate', e);
                    }
                }
            }
        }
    }, []);

    // Serialized FIFO drain: guarantees in-order, lossless processing even
    // when several messages land before the first one finishes its async
    // work (so `setRemoteDescription` always precedes the `addIceCandidate`
    // calls for that generation).
    const drainInbound = useCallback(async () => {
        if (drainingRef.current) return;
        drainingRef.current = true;
        try {
            while (inboundQueueRef.current.length > 0) {
                const message = inboundQueueRef.current.shift()!;
                try {
                    await handleSignalingMessage(message);
                } catch (e) {
                    console.error('[WebRTC] signaling handler error', e);
                }
            }
        } finally {
            drainingRef.current = false;
        }
    }, [handleSignalingMessage]);

    // Subscribe to the lossless signaling stream and feed the FIFO.
    useEffect(() => {
        if (!deskId) return;
        const unsubscribe = subscribe((message) => {
            inboundQueueRef.current.push(message);
            void drainInbound();
        });
        return unsubscribe;
    }, [subscribe, deskId, drainInbound]);

    const connect = useCallback(async (settings: any) => {
        if (!initData || !deskId) return;

        console.log('Connecting with settings', settings);

        if (peerConnection.current) {
            peerConnection.current.close();
        }

        // Fresh PeerConnection: roll the coordinator's epoch (invalidating
        // any prior PC's late callbacks) and reset the retry budget.
        const coordinator = coordinatorRef.current;
        if (!coordinator) return;
        coordinator.resetForNewPc();
        const epoch = coordinator.currentEpoch();

        const pc = new RTCPeerConnection({
            iceServers: initData.ice_servers || [],
        });
        peerConnection.current = pc;
        // Fresh attempt: clear any terminal failure from a previous connection.
        setRtcFailed(false);

        pc.onicecandidate = (event) => {
            if (event.candidate !== null) {
                console.log("[WebRTC] Send ICE canididate:", event.candidate);
                sendMessage(SIGNALING_TYPE_CODE_ICE_CANDIDATE, event.candidate, deskId);
            }
        };

        pc.oniceconnectionstatechange = () => {
            console.log(`[WebRTC] ICE Connection State changed to: ${pc.iceConnectionState}`);
            // Ignore callbacks from a PeerConnection that has since been
            // replaced (epoch moved on).
            if (epoch !== coordinator.currentEpoch()) return;
            if (pc.iceConnectionState === 'disconnected') {
                // Transient: mark the link down but NOT failed — ICE usually
                // heals on its own, so the UI must not reopen the config
                // dialog. The coordinator deliberately does not retry on
                // `disconnected`.
                setIsRTCConnected(false);
                return;
            }
            // `connected`/`completed`/`failed` drive the coordinator, which
            // owns the connected/failed UI state and the auto-retry on
            // `failed`.
            coordinator.onIceStateChange(pc.iceConnectionState);
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
        await createAndSetStereoOffer(pc);

        // Cache the immutable OfferModel so the coordinator can re-send it
        // verbatim on an awaiting-answer timeout (signaling loss).
        const offerModel = {
            offer: pc.localDescription,
            desk_settings: settings,
        };
        rtcDeps.current.settings = settings;
        rtcDeps.current.cachedOfferModel = offerModel;

        // Send Offer immediately (Trickle ICE). The ANSWER watchdog is armed
        // by the coordinator only once this OFFER actually reaches the wire
        // (`onSent`), so a queued-while-offline OFFER never burns a retry.
        const requestId = coordinator.beginOffer();
        sendTracked({
            type: SIGNALING_TYPE_CODE_OFFER,
            data: offerModel,
            toConnectionId: deskId ?? undefined,
            requestId,
            replaceKey: offerReplaceKey(deskId),
            onSent: (id) => coordinator.markOfferSent(id),
        });

    }, [initData, deskId, sendMessage, sendTracked]);

    const renegotiate = useCallback(async (settings: any) => {
        const pc = peerConnection.current;
        if (!pc || pc.signalingState !== 'stable') {
            // An initial Offer rejection leaves the PC in `have-local-offer`;
            // only a fresh PC can recover that case safely.
            await connect(settings);
            return;
        }

        await createAndSetStereoOffer(pc);
        const offerModel = {
            offer: pc.localDescription,
            desk_settings: settings,
        };
        rtcDeps.current.settings = settings;
        rtcDeps.current.cachedOfferModel = offerModel;

        const coordinator = coordinatorRef.current;
        if (!coordinator) return;
        const requestId = coordinator.beginOffer();
        sendTracked({
            type: SIGNALING_TYPE_CODE_OFFER,
            data: offerModel,
            toConnectionId: deskId ?? undefined,
            requestId,
            replaceKey: offerReplaceKey(deskId),
            onSent: (id) => coordinator.markOfferSent(id),
        });
    }, [connect, deskId, sendTracked]);

    const closeRTC = useCallback(() => {
        // Tear down auto-retry first: bump the epoch so any in-flight
        // callback/timer is inert, then purge a still-queued OFFER so a later
        // signaling reconnect doesn't replay a stale negotiation.
        coordinatorRef.current?.dispose();
        cancelQueued(offerReplaceKey(rtcDeps.current.deskId));
        if (peerConnection.current) {
            peerConnection.current.close();
            peerConnection.current = null;
        }
        setIsRTCConnected(false);
        setRemoteStream(null);
    }, [cancelQueued]);

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
        renegotiate,
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
