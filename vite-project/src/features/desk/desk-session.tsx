import { useEffect, useRef, useState, useCallback, useMemo } from "react"
import { useParams, useNavigate } from "react-router-dom"
import { useTranslation } from "react-i18next"
import { AlertTriangle, Loader2 } from "lucide-react"
import { TooltipProvider } from "@/components/ui/tooltip"

import { Input } from "@/components/ui/input"
import "./desk-session.css"
import { useDeskSignaling } from "./use-desk-signaling"
import type { SignalingMessage } from "./use-desk-signaling"
import { useDeskRTC } from "./use-desk-rtc"
import { useDeskDiagnose } from "./use-desk-diagnose"
import { DiagnosePanel } from "./diagnose-panel"
import { useConfirmExec } from "../exec/use-confirm-exec"
import { useDeskInput } from "./use-desk-input"
import { lockEscapeKey, unlockKeyboard, isKeyboardLockSupported } from "./fullscreen-keyboard"
import { useBeforeUnloadConfirm } from "./use-before-unload-confirm"
import { useDeskClipboard } from "./use-desk-clipboard"
import { useDeskWhiteboard } from "./use-desk-whiteboard"
import { useCursorSync } from "./use-cursor-sync"
import WhiteboardCanvas from "./whiteboard-canvas"
import WhiteboardToolbar from "./whiteboard-toolbar"
import { useDeskMicrophone } from "./use-desk-microphone"
import { DeskConfigDialog } from "./desk-config-dialog"
import { useAdaptiveResolution, isAdaptiveResolutionGateOpen } from "./use-adaptive-resolution"
import { useResolutionToast } from "./use-resolution-toast"
import { isWebRtcAvailable } from "./webrtc-support"
import { useToast } from "@/hooks/use-toast"
import type { DeskSettings } from "@/services/types"
import { deskErrorCodeEnum } from "@/services/types"
import { useRestrictedSession } from "@/features/desk/restricted-session"
import {
    buildDesktopRequestRemotePayload,
    shouldOpenConfigDialog,
} from "./desk-session-model"
import {
    ClipboardFallbackToast,
    ConnectionQualityBadge,
    DeskSessionStats,
    ResolutionStatusToast,
} from "./desk-session-panels"
import { DeskControlBar } from "./desk-control-bar"
import { useDraggableControlBar } from "./use-draggable-control-bar"
import {
    SIGNALING_TYPE_CODE_REQUEST_REMOTE,
    SIGNALING_TYPE_CODE_REQUIRE_CONTROL,
    SIGNALING_TYPE_CODE_CLOSE_CONTROL,
    SIGNALING_TYPE_CODE_ACCEPT_CONTROL,
    SIGNALING_TYPE_CODE_DENY_CONTROL,
    SIGNALING_TYPE_CODE_UPDATE_DESK_SETTINGS,
    SIGNALING_TYPE_CODE_ENABLE_PRIVATE_SCREEN,
    SIGNALING_TYPE_CODE_PRIVATE_SCREEN_STATE_CHANGED,
    SIGNALING_TYPE_CODE_AUDIO_PLAYBACK_ERROR,
    SIGNALING_TYPE_CODE_CHANGE_DISPLAY_SETTINGS,
    SIGNALING_TYPE_CODE_ERROR,
    SIGNALING_TYPE_CODE_INIT,
} from "./constants"
import { AdmissionRetrySchedule } from "./admission-retry"
import { usePrivateScreenPending } from "./use-private-screen-pending"

/** Container props. `orgId` is injected only by the manager console's org view
 *  (via a static wrapper); the open-source standalone app renders `<DeskSession/>`
 *  with no props, keeping the AI model selection personal-scoped. */
type DeskSessionProps = {
    orgId?: number
}

export default function DeskSession({ orgId }: DeskSessionProps = {}) {
    const { id: deskId } = useParams<{ id: string }>()
    const navigate = useNavigate()
    const { t } = useTranslation()
    const { toast } = useToast()

    // Control state
    const [hasControl, setHasControl] = useState(false);
    const [isWaitingApproval, setIsWaitingApproval] = useState(false);
    const hasRequestedRef = useRef(false);
    const admissionRetryRef = useRef({
        generation: 0,
        requestIds: new Set<string>(),
        schedule: new AdmissionRetrySchedule(),
        timer: null as number | null,
    });

    const { isConnected, subscribe, sendMessage, sendTracked, cancelQueued } = useDeskSignaling()

    // Restriction state derived from the redeemed grant (if any) for this target.
    const restricted = useRestrictedSession(deskId);
    const grantSessionId = restricted.grantSessionId;

    const clearAdmissionRetry = useCallback(() => {
        const state = admissionRetryRef.current;
        state.generation += 1;
        state.requestIds.clear();
        state.schedule.reset();
        if (state.timer !== null) {
            window.clearTimeout(state.timer);
            state.timer = null;
        }
    }, []);

    const sendRemoteAdmission = useCallback((newLogicalAttempt: boolean) => {
        if (!deskId) return;
        const state = admissionRetryRef.current;
        if (newLogicalAttempt) {
            clearAdmissionRetry();
        }
        const requestData = buildDesktopRequestRemotePayload(deskId, grantSessionId)
        const requestId = sendMessage(
            SIGNALING_TYPE_CODE_REQUEST_REMOTE,
            requestData,
            deskId,
        );
        state.requestIds.add(requestId);
        hasRequestedRef.current = true;
    }, [clearAdmissionRetry, deskId, grantSessionId, sendMessage]);

    const handleConnect = useCallback(() => {
        if (deskId && !hasRequestedRef.current) {
            console.log("WebSocket opened, requesting remote connection directly:", deskId);
            sendRemoteAdmission(true);
        }
    }, [deskId, sendRemoteAdmission]);

    useEffect(() => {
        if (isConnected) {
            handleConnect();
        }
    }, [isConnected, handleConnect]);
    const videoRef = useRef<HTMLVideoElement>(null)
    const videoWrapperRef = useRef<HTMLDivElement>(null)
    const controlBarRef = useRef<HTMLDivElement>(null)

    const [isConfigOpen, setIsConfigOpen] = useState(false);
    // True once the user has kicked off a WebRTC connect from the dialog. Gates
    // the auto-reopen so a transient ICE `disconnected` during/after negotiation
    // does not pop the dialog back up over a connection that is still healing.
    const hasAttemptedConnectRef = useRef(false);
    const [isVideoReady, setIsVideoReady] = useState(false);
    const [isMuted, setIsMuted] = useState(() => {
        // Safari/iOS requires muted for autoPlay
        return /Mobile|Android|iP(ad|hone)/.test(navigator.userAgent) ? true : false;
    });
    const [audioVolume, setAudioVolume] = useState(100);
    const [isFullscreen, setIsFullscreen] = useState(false);
    // Whether this environment can capture Escape via the Keyboard Lock API
    // (Chromium + secure context). When it cannot, Escape is swallowed by the
    // browser in fullscreen, so we expose Esc in the shortcut menu and warn the
    // user. Stable for the session.
    const keyboardLockSupported = useMemo(() => isKeyboardLockSupported(), []);
    const [showEscHint, setShowEscHint] = useState(false);

    const { handleDragStart, isDragging } = useDraggableControlBar({
        controlBarRef,
        wrapperRef: videoWrapperRef,
    })

    const [showStats, setShowStats] = useState(false);
    const [showDiagnose, setShowDiagnose] = useState(false);

    // Adaptive-quality opt-in (client-only, persisted in localStorage so
    // the user's preference survives reloads). When `false` the
    // packet-loss / RTT driven encoder-rebuild loop below short-circuits
    // and `video_quality` stays at whatever the config dialog last sent.
    // Default `true` preserves the historical behaviour.
    const [adaptiveQualityEnabled, setAdaptiveQualityEnabled] = useState<boolean>(() => {
        try {
            const raw = localStorage.getItem("lcxl-desk-adaptive-quality-enabled");
            return raw === null ? true : raw === "true";
        } catch {
            return true;
        }
    });
    useEffect(() => {
        try {
            localStorage.setItem("lcxl-desk-adaptive-quality-enabled", String(adaptiveQualityEnabled));
        } catch {
            // Ignore quota / private-mode errors — runtime state is still
            // correct, only persistence is lost.
        }
    }, [adaptiveQualityEnabled]);

    // Server-side adaptive bitrate-cap opt-in (REMB-driven inner loop
    // on the daemon). Browser-owned preference, persisted like the
    // adaptive-quality toggle above; injected into
    // `DeskSettings.adaptive_bitrate` on connect / UpdateDeskSettings
    // (the server treats it as session state and never persists it).
    const [adaptiveBitrateEnabled, setAdaptiveBitrateEnabled] = useState<boolean>(() => {
        try {
            const raw = localStorage.getItem("lcxl-desk-adaptive-bitrate-enabled");
            return raw === null ? true : raw === "true";
        } catch {
            return true;
        }
    });
    useEffect(() => {
        try {
            localStorage.setItem("lcxl-desk-adaptive-bitrate-enabled", String(adaptiveBitrateEnabled));
        } catch {
            // Ignore quota / private-mode errors (see above).
        }
    }, [adaptiveBitrateEnabled]);

    // Privacy screen state
    const [isPrivateScreen, setIsPrivateScreen] = useState(false);
    const [isPrivateScreenSupported, setIsPrivateScreenSupported] = useState(true);
    const {
        pending: isPrivateScreenPending,
        start: startPrivateScreenPending,
        confirm: confirmPrivateScreenPending,
        fail: failPrivateScreenPending,
        clear: clearPrivateScreenPending,
    } = usePrivateScreenPending({
        onError: (kind, message) => {
            toast({
                variant: "destructive",
                title: t(kind === "timeout"
                    ? "pages.desk.privateScreenTimeout"
                    : "pages.desk.privateScreenFailed"),
                description: message,
            });
        },
    });

    const { peerConnection, remoteStream, initData, connect, mouseChannel, keyboardChannel, mouseMoveChannel, clipboardChannel, whiteboardChannel, cursorSyncChannel, isRTCConnected, rtcFailed, closeRTC, rtcStats } = useDeskRTC({
        deskId: deskId || null,
        subscribe,
        sendMessage,
        sendTracked,
        cancelQueued
    });

    // Guard against accidentally closing/reloading an active session.
    useBeforeUnloadConfirm(isRTCConnected);

    // AI diagnose stream: sends `Diagnose` and aggregates `DiagnoseEvent`
    // frames off the same signaling channel.
    const diagnose = useDeskDiagnose({
        deskId: deskId || null,
        subscribe,
        sendMessage,
    });

    // Confirmed execution of a suggested command: ConfirmExec -> ExecPreview ->
    // ResolveExec -> ExecResult, keyed by command row.
    const exec = useConfirmExec({
        deskId: deskId || null,
        subscribe,
        sendMessage,
        orgId,
    });

    // The daemon keeps the WebRTC PC alive across worker
    // swaps. The desktop-switch reconnect state machine
    // (`reconnectTimedOut`, `autoReconnectRef`, `switchTimeoutRef`) is
    // gone — the browser sees worker swaps as a brief frame freeze that
    // resolves on the next IDR, with no React-level state to manage.
    // `lastSettingsRef` survives because the non-reconnect parts of the
    // adaptive-quality loop still consult it.
    const lastSettingsRef = useRef<DeskSettings | null>(null);
    // State mirror of the user-submitted settings used solely for
    // hooks/effects whose React-tracked deps must re-evaluate on a
    // settings change. Refs are intentionally invisible to React's
    // render cycle, so reading `lastSettingsRef.current` inside a hook's
    // `enabled` prop would silently miss "user changed display to the
    // IDD and ticked adaptive" until some other state happened to
    // re-render the component. Mirror only fields the gating cares
    // about — the adaptive-quality auto-adjust path keeps writing only
    // to the ref so its per-stats-tick updates do not force a re-render.
    const [activeSettings, setActiveSettings] = useState<DeskSettings | null>(null);

    // Adaptive resolution: request ids the hook has emitted but not yet
    // seen an echo for. The control-signaling subscription uses this set as
    // a membership check to detect auto-resolution echoes (so manual path
    // echoes from future UI keep working unchanged), while the
    // `useResolutionToast` hook below drives the right-bottom toast state
    // machine off the same signaling subscription.
    const pendingAutoRequestIdsRef = useRef<Set<string>>(new Set());

    // Adaptive-resolution status toast (right-bottom corner). Lives in
    // its own hook so the state machine can be exercised in isolation:
    //   - updating → success / failed transitions guarded by the
    //     latest registered request id (stale echoes are dropped)
    //   - 15 s watchdog promotes a never-acked `updating` to `failed`
    //   - flipping `isRTCConnected` to false clears a stuck toast
    // The `translate` closure is hand-rolled instead of passing `t`
    // directly to keep the hook framework-agnostic for testing.
    const { resolutionToast, registerSent: registerResolutionSent } =
        useResolutionToast({
            subscribe,
            isRTCConnected,
            changeDisplaySettingsType: SIGNALING_TYPE_CODE_CHANGE_DISPLAY_SETTINGS,
            translate: (key) => t(key),
        });

    // Adaptive quality state
    const statsWindowRef = useRef<Array<{ packetLoss: number; rtt: number }>>([]);
    const lastQualityAdjustRef = useRef<number>(0);
    // How many times the adaptive loop has rebuilt the encoder so far.
    // Surfaced in the metrics panel as the headline "is the controller
    // oscillating?" signal — a healthy run converges to a stable value
    // and the count grows slowly; a thrashing run pumps this every few
    // seconds. Lives in a ref because the stats panel re-renders every
    // second on `rtcStats` updates already, so reading the ref there
    // is fresh without an extra setState round-trip.
    const qualityAdjustmentCountRef = useRef<number>(0);

    const { cursorStyle } = useCursorSync(cursorSyncChannel, videoRef, isRTCConnected && hasControl);

    const {
        clipboardEnabled,
        transferProgress,
        transferStatus,
        errorMessage,
        toggleClipboard,
        fallbackToast,
        execFallbackToastAction,
        closeFallbackToast
    } = useDeskClipboard({
        clipboardChannel,
        hasControl: isRTCConnected && hasControl,
        isActive: true
    });

    const whiteboard = useDeskWhiteboard({
        videoRef,
        whiteboardChannel,
        isConnected: isRTCConnected && hasControl,
        hasTauri: initData?.has_tauri ?? false,
    });

    const { sendKeyboardEvents } = useDeskInput({
        videoRef,
        mouseChannel,
        keyboardChannel,
        mouseMoveChannel,
        isConnected: isRTCConnected && hasControl, // Only enable inputs if we have control
        ignoreInputEvents: !!whiteboard.textInput,
    });

    const microphone = useDeskMicrophone({
        peerConnection,
        isConnected: isRTCConnected,
    });

    const { forceError } = microphone;

    // `forceError` (from the microphone hook) changes identity ~1 Hz with
    // the rtcStats pulse; route it through a ref so the control-signaling
    // subscription below stays stable instead of re-registering each tick.
    const forceErrorRef = useRef(forceError);
    forceErrorRef.current = forceError;

    // Handle incoming control-related signaling messages off the lossless
    // subscription stream (every message delivered in order, none coalesced).
    useEffect(() => {
        const handle = (message: SignalingMessage) => {
            const { signaling_type } = message;

            if (signaling_type === SIGNALING_TYPE_CODE_INIT
                && message.request_id
                && admissionRetryRef.current.requestIds.has(message.request_id)
            ) {
                clearAdmissionRetry();
            } else if (signaling_type === SIGNALING_TYPE_CODE_ERROR
                && message.request_id
                && admissionRetryRef.current.requestIds.delete(message.request_id)
            ) {
                if (message.response_state?.error_code === deskErrorCodeEnum.ACTION_NEED_RETRY) {
                    const state = admissionRetryRef.current;
                    const delay = state.schedule.nextDelay();
                    if (delay === null) {
                        clearAdmissionRetry();
                        toast({
                            title: t('pages.desk.admissionRetry.title'),
                            description: t('pages.desk.admissionRetry.exhausted'),
                            variant: 'destructive',
                        });
                    } else {
                        const generation = state.generation;
                        if (state.timer !== null) window.clearTimeout(state.timer);
                        state.timer = window.setTimeout(() => {
                            if (admissionRetryRef.current.generation === generation) {
                                admissionRetryRef.current.timer = null;
                                sendRemoteAdmission(false);
                            }
                        }, delay);
                    }
                } else {
                    clearAdmissionRetry();
                }
            } else if (signaling_type === SIGNALING_TYPE_CODE_ACCEPT_CONTROL) {
                console.log("Remote control request ACCEPTED by peer.");
                setHasControl(true);
                setIsWaitingApproval(false);
                videoRef.current?.focus();
            } else if (signaling_type === SIGNALING_TYPE_CODE_DENY_CONTROL) {
                console.log("Remote control request DENIED by peer.");
                setHasControl(false);
                setIsWaitingApproval(false);
            } else if (signaling_type === SIGNALING_TYPE_CODE_CLOSE_CONTROL) {
                console.log("Remote control CLOSED by peer.");
                setHasControl(false);
                setIsWaitingApproval(false);
            } else if (signaling_type === SIGNALING_TYPE_CODE_PRIVATE_SCREEN_STATE_CHANGED) {
                const data = message.signaling_data;
                if (data) {
                    console.log("Private screen state changed:", data);
                    setIsPrivateScreen(data.visible ?? false);
                    setIsPrivateScreenSupported(data.is_supported ?? true);
                    if (data.error_msg) {
                        console.error("Private screen error:", data.error_msg);
                        failPrivateScreenPending(data.error_msg);
                    } else if (typeof data.visible === "boolean") {
                        confirmPrivateScreenPending(data.visible);
                    }
                }
            } else if (signaling_type === SIGNALING_TYPE_CODE_AUDIO_PLAYBACK_ERROR) {
                const data = message.signaling_data;
                if (data && data.error) {
                    console.error("Remote audio playback error:", data.error);
                    forceErrorRef.current(data.error);
                }
            } else if (signaling_type === SIGNALING_TYPE_CODE_CHANGE_DISPLAY_SETTINGS) {
                // Adaptive-resolution echo: the right-bottom status toast
                // is driven by `useResolutionToast`, which subscribes to
                // the signaling stream directly and gates transitions by the
                // most recent request id. Here we only need to drain
                // `pendingAutoRequestIdsRef` so the membership-tracking
                // contract used by future manual ChangeDisplaySettings UI
                // does not leak.
                const requestId = message.request_id;
                if (requestId && pendingAutoRequestIdsRef.current.delete(requestId)) {
                    console.debug("[adaptive-resolution] response", message);
                }
            }
        };
        return subscribe(handle);
    }, [clearAdmissionRetry, confirmPrivateScreenPending, failPrivateScreenPending, sendRemoteAdmission, subscribe, t, toast]);

    // Reset requested state if connection drops
    useEffect(() => {
        if (!isConnected) {
            clearAdmissionRetry();
            clearPrivateScreenPending();
            hasRequestedRef.current = false;
        }
    }, [clearAdmissionRetry, clearPrivateScreenPending, isConnected]);

    useEffect(() => clearAdmissionRetry, [clearAdmissionRetry]);

    // Wait for INIT data, then show the config dialog so the user can pick
    // capture settings. Reopen it only for the initial pick or after a terminal
    // ICE failure (retry) — never on a transient `disconnected`, which heals on
    // its own (see `shouldOpenConfigDialog`). The auto-reconnect path that fired
    // after `DesktopReady` is gone — the daemon-held PC survives worker swaps so
    // INIT only ever arrives once per session.
    useEffect(() => {
        if (
            shouldOpenConfigDialog({
                hasInitData: !!initData,
                isRTCConnected,
                hasAttemptedConnect: hasAttemptedConnectRef.current,
                rtcFailed,
            })
        ) {
            console.log("Showing config dialog for remote connection");
            setIsConfigOpen(true);
        }
    }, [initData, isRTCConnected, rtcFailed]);

    // Once the media link is up, close the config dialog. Guards against a
    // dialog that was reopened by an earlier not-connected state lingering in
    // front of the now-playing remote video.
    useEffect(() => {
        if (isRTCConnected) {
            setIsConfigOpen(false);
        }
    }, [isRTCConnected]);

    // Attach remote stream to video element
    useEffect(() => {
        if (videoRef.current && remoteStream && isRTCConnected) {
            console.log("[Video] WebRTC is connected. Setting srcObject with remoteStream. Tracks:", remoteStream.getTracks().map(t => t.kind));

            // Only assign if it hasn't been assigned yet, to avoid React infinite stream interruption
            if (videoRef.current.srcObject !== remoteStream) {
                videoRef.current.srcObject = remoteStream;
            }

            videoRef.current.play().then(() => {
                console.log("[Video] Successfully started playing implicitly.");
            }).catch(e => {
                console.warn("[Video] Failed to implicitly play video stream on mount, user interaction might be required on this browser (Safari iOS): ", e);
            });
        }
    }, [remoteStream, isRTCConnected]);

    // Debugging video element state
    useEffect(() => {
        if (!isConnected || !videoRef.current) return;
        const interval = setInterval(() => {
            const v = videoRef.current;
            if (v) {
                //console.log(`[Video State] readyState: ${v.readyState}, paused: ${v.paused}, muted: ${v.muted}, videoWidth: ${v.videoWidth}, videoHeight: ${v.videoHeight}, srcObject: ${!!v.srcObject}`);
            }
        }, 2000);
        return () => clearInterval(interval);
    }, [isConnected]);

    // Adaptive quality: adjust video_quality based on packet loss and RTT
    useEffect(() => {
        if (!isRTCConnected || !deskId || !lastSettingsRef.current) return;
        // User-toggleable: when disabled, no UpdateDeskSettings is sent
        // from this loop and the encoder is never rebuilt for ABR
        // reasons.
        if (!adaptiveQualityEnabled) {
            statsWindowRef.current = [];
            return;
        }

        // Do not trigger adaptive quality if we haven't received any data yet or the stream is currently paused/stalled
        if (rtcStats.fps === 0 && rtcStats.bitrate === 0) {
            statsWindowRef.current = []; // reset window to avoid acting on stale/initial 0-stats
            return;
        }

        const win = statsWindowRef.current;
        win.push({ packetLoss: rtcStats.packetLoss, rtt: rtcStats.rtt });
        if (win.length > 10) win.shift();
        if (win.length < 3) return;

        const avgPacketLoss = win.reduce((s, x) => s + x.packetLoss, 0) / win.length;
        const avgRtt = win.reduce((s, x) => s + x.rtt, 0) / win.length;
        const now = Date.now();
        const elapsed = now - lastQualityAdjustRef.current;
        const currentQuality = lastSettingsRef.current.video_quality ?? 22;

        let newQuality: number | null = null;
        if ((avgPacketLoss > 3 || avgRtt > 200) && elapsed >= 3000) {
            newQuality = Math.min(63, currentQuality + 5);
        } else if (avgPacketLoss < 0.5 && avgRtt < 100 && elapsed >= 10000) {
            newQuality = Math.max(0, currentQuality - 2);
        }

        if (newQuality !== null && newQuality !== currentQuality) {
            const newSettings = { ...lastSettingsRef.current, video_quality: newQuality };
            lastSettingsRef.current = newSettings;
            sendMessage(SIGNALING_TYPE_CODE_UPDATE_DESK_SETTINGS, newSettings, deskId);
            lastQualityAdjustRef.current = now;
            statsWindowRef.current = [];
            qualityAdjustmentCountRef.current += 1;
        }
    }, [rtcStats, adaptiveQualityEnabled, sendMessage, deskId]);

    // Adaptive resolution dispatcher: wraps sendMessage so the hook
    // gets the real wire request_id back (sendMessage returns it).
    // `connection_id` defaults to
    // `deskId` because the daemon's auto path keys per-connection.
    // After the send is queued we hand the same id to
    // `useResolutionToast.registerSent` so the status toast switches
    // to "updating" immediately and the watchdog starts ticking —
    // even when the request never reaches the daemon (transport down),
    // the watchdog will eventually surface a timeout instead of
    // leaving the operator staring at a frozen spinner.
    const sendChangeDisplay = useCallback(
        (payload: {
            width: number;
            height: number;
            refresh_hz: number;
            auto: true;
        }) => {
            const reqId = sendMessage(
                SIGNALING_TYPE_CODE_CHANGE_DISPLAY_SETTINGS,
                payload,
                deskId ?? undefined,
            );
            console.info("[adaptive-resolution dispatch] 205 sent", {
                reqId,
                payload,
                deskId,
            });
            registerResolutionSent(reqId, payload.width, payload.height);
            return reqId;
        },
        [sendMessage, deskId, registerResolutionSent],
    );

    // The hook's `enabled` aggregates every condition that must be
    // satisfied for auto-resolution to make sense:
    //   - deskId is real (so sendMessage has a connection target)
    //   - WebRTC is actually connected (RTCPeerConnection up + tracks
    //     flowing — there is no point adapting an inactive stream)
    //   - daemon side reports the IDD is currently attached
    //     (`virtual_display_active`); without this the daemon would
    //     reject every auto request with FEATURE_UNAVAILABLE
    //   - daemon side surfaces the IDD's GDI name
    //     (`virtual_display_device_name`) AND that name matches the
    //     display the worker is actually capturing. If the operator
    //     picked a physical monitor in the config dialog, firing 205
    //     would silently change the IDD's resolution while WGC keeps
    //     capturing the physical screen — invisible to the user. The
    //     config dialog now disables the adaptive toggle in this
    //     scenario, but defence-in-depth here keeps us safe if a
    //     stale `adaptive_web_page_resolution=true` slips through.
    //   - user toggled "Adaptive Resolution" on in the config dialog
    // `lastSettingsRef.current` is populated from `handleConfigSubmit`
    // before we ever call `connect`, so reading it here after RTC is
    // up is always safe.
    const adaptiveGateInputs = {
        deskId,
        isRTCConnected,
        virtualDisplayActive: initData?.virtual_display_active,
        virtualDisplayDeviceName: initData?.virtual_display_device_name,
        selectedVideoDeviceName: activeSettings?.video_device_name,
        adaptiveWebPageResolution: activeSettings?.adaptive_web_page_resolution,
    };
    const adaptiveGateOpen = isAdaptiveResolutionGateOpen(adaptiveGateInputs);
    // Diagnostic: log every gate evaluation. Each axis is dumped so the
    // operator can see exactly which check is closed (the daemon-side
    // active flag missing, the selected device not matching the IDD,
    // the user toggle off, etc.). Grep `[adaptive-resolution gate]` in
    // devtools. The effect's dep list is the spread of the inputs so it
    // runs only on a real change, not on every render.
    useEffect(() => {
        console.info("[adaptive-resolution gate]", {
            ...adaptiveGateInputs,
            deviceMatch:
                adaptiveGateInputs.selectedVideoDeviceName ===
                adaptiveGateInputs.virtualDisplayDeviceName,
            open: adaptiveGateOpen,
        });
    }, [
        adaptiveGateInputs.deskId,
        adaptiveGateInputs.isRTCConnected,
        adaptiveGateInputs.virtualDisplayActive,
        adaptiveGateInputs.virtualDisplayDeviceName,
        adaptiveGateInputs.selectedVideoDeviceName,
        adaptiveGateInputs.adaptiveWebPageResolution,
        adaptiveGateOpen,
    ]);
    useAdaptiveResolution({
        wrapperRef: videoWrapperRef,
        enabled: adaptiveGateOpen,
        sendChangeDisplay,
        pendingAutoRequestIds: pendingAutoRequestIdsRef,
        // `bigint` (u64 on the wire) → `number` because setTimeout
        // does not accept bigint. The clamp on the daemon side keeps
        // the value comfortably inside Number's safe-integer range.
        debounceMs:
            initData?.adaptive_resolution?.debounce_ms !== undefined
                ? Number(initData.adaptive_resolution.debounce_ms)
                : undefined,
        minDeltaPx: initData?.adaptive_resolution?.min_delta_px ?? undefined,
    });

    const handleConfigSubmit = (settings: DeskSettings) => {
        // Some webviews (notably Linux WebKitGTK with WebRTC disabled) lack
        // RTCPeerConnection. Surface a clear message instead of letting the
        // connection attempt throw an unhandled rejection that the user never
        // sees.
        if (!isWebRtcAvailable()) {
            toast({
                variant: "destructive",
                title: t("pages.desk.webrtcUnavailableTitle"),
                description: t(
                    "pages.desk.webrtcUnavailableDesc",
                ),
            });
            return;
        }
        // Inject the parent-owned adaptive-bitrate preference so it
        // rides the offer (new connection init) and UpdateDeskSettings
        // (live toggle for this connection on the daemon side).
        const settingsWithPrefs: DeskSettings = {
            ...settings,
            adaptive_bitrate: adaptiveBitrateEnabled,
        };
        lastSettingsRef.current = settingsWithPrefs;
        // Mirror to state so `useAdaptiveResolution` re-evaluates its
        // `enabled` gate on this submit even when no other tracked
        // state happens to change in the same tick (e.g. the user is
        // already connected and only flips display + adaptive toggle).
        setActiveSettings(settingsWithPrefs);
        if (isRTCConnected && deskId) {
            console.log("Updating desk settings dynamically...", settingsWithPrefs);
            sendMessage(SIGNALING_TYPE_CODE_UPDATE_DESK_SETTINGS, settingsWithPrefs, deskId);
            toast({ title: t("pages.desk.settingsSent") });
        } else {
            // Mark that a connect is in flight so a transient ICE drop during
            // negotiation does not auto-reopen this dialog.
            hasAttemptedConnectRef.current = true;
            connect(settingsWithPrefs);
        }
        setIsConfigOpen(false);
    };

    // Live-apply the adaptive-bitrate toggle while connected: the
    // checkbox sits outside the dialog form, so a flip must reach the
    // daemon without waiting for the next full settings submit. The
    // daemon scopes the change to this connection.
    useEffect(() => {
        if (!isRTCConnected || !deskId || !lastSettingsRef.current) return;
        if (lastSettingsRef.current.adaptive_bitrate === adaptiveBitrateEnabled) return;
        const updated: DeskSettings = {
            ...lastSettingsRef.current,
            adaptive_bitrate: adaptiveBitrateEnabled,
        };
        lastSettingsRef.current = updated;
        sendMessage(SIGNALING_TYPE_CODE_UPDATE_DESK_SETTINGS, updated, deskId);
    }, [adaptiveBitrateEnabled, isRTCConnected, deskId, sendMessage]);

    const handleConfigCancel = () => {
        setIsConfigOpen(false);
        navigate(`/desk/${deskId}`);
    };

    const handleRequestControl = () => {
        // In a restricted session, only auto-request the capabilities the code's
        // ceiling does not deny; an owner session leaves every dimension visible so
        // this keeps the previous unconditional behaviour.
        const wantClipboard = !hasControl && restricted.capabilityVisible('allow_clipboard_sync');
        const wantFileTransfer = !hasControl && restricted.capabilityVisible('allow_file_transfer');
        const requestControlData = {
            accept: !hasControl,
            accept_clipboard_sync: wantClipboard, // Auto-request clipboard when the ceiling allows it
            accept_file_transfer: wantFileTransfer,
        };
        // Auto-enable UI state if asking for control
        if (wantClipboard && window.isSecureContext !== false) {
            if (!clipboardEnabled) toggleClipboard();
        }

        console.log(`Sending REQUIRE_CONTROL signaling, requestControlData:`, requestControlData);
        sendMessage(SIGNALING_TYPE_CODE_REQUIRE_CONTROL, requestControlData, deskId);
        setIsWaitingApproval(true);
    };

    const handleDisconnect = () => {
        clearPrivateScreenPending();
        if (deskId) {
            if (isPrivateScreen) {
                console.log(`Disabling private screen before disconnect`);
                sendMessage(SIGNALING_TYPE_CODE_ENABLE_PRIVATE_SCREEN, { enable: false }, deskId);
            }
            sendMessage(SIGNALING_TYPE_CODE_CLOSE_CONTROL, null, deskId);
        }
        closeRTC();
        navigate(`/desk/${deskId}`);
    };

    const handleTogglePrivateScreen = () => {
        if (!deskId) return;
        const newState = !isPrivateScreen;
        if (!startPrivateScreenPending(newState)) return;
        console.log(`Toggling private screen: ${newState}`);
        sendMessage(SIGNALING_TYPE_CODE_ENABLE_PRIVATE_SCREEN, { enable: newState }, deskId);
    };

    const handleFullscreen = async () => {
        if (!document.fullscreenElement) {
            try {
                await videoWrapperRef.current?.requestFullscreen();
            } catch (err) {
                console.log(`Error attempting to enable fullscreen: ${(err as Error).message}`);
                return;
            }
            // Lock Escape only after fullscreen is actually active, otherwise
            // Chromium does not engage the press-and-hold-to-exit behaviour and
            // keeps swallowing Escape (so the host never receives it).
            await lockEscapeKey();
        } else {
            // Releasing the lock is handled by the fullscreenchange listener so
            // it also covers exiting via press-and-hold Escape.
            document.exitFullscreen();
        }
    };

    // Keep fullscreen state in sync
    useEffect(() => {
        const handleFullscreenChange = () => {
            setIsFullscreen(!!document.fullscreenElement);
            // Release the Escape keyboard lock whenever we leave fullscreen,
            // including the press-and-hold-Escape exit path that never goes
            // through handleFullscreen.
            if (!document.fullscreenElement) {
                unlockKeyboard();
            }
            // Re-center control bar if it exists
            if (controlBarRef.current) {
                const cb = controlBarRef.current;
                cb.style.transition = 'none';
                cb.style.left = '';
                cb.style.top = '';
                cb.style.bottom = '';
                cb.style.transform = '';
            }
        };
        document.addEventListener('fullscreenchange', handleFullscreenChange);
        return () => document.removeEventListener('fullscreenchange', handleFullscreenChange);
    }, []);

    // When entering fullscreen in an environment that cannot capture Escape,
    // briefly remind the user that Esc must be sent via the shortcut menu.
    useEffect(() => {
        if (!isFullscreen || keyboardLockSupported) {
            setShowEscHint(false);
            return;
        }
        setShowEscHint(true);
        const timer = setTimeout(() => setShowEscHint(false), 6000);
        return () => clearTimeout(timer);
    }, [isFullscreen, keyboardLockSupported]);

    const handleVolumeChange = (value: number) => {
        if (!videoRef.current) return;
        setAudioVolume(value);
        videoRef.current.volume = value / 100;
        setIsMuted(value === 0);
    };

    return (
        <div className="flex h-full flex-col">
            <DeskConfigDialog
                open={isConfigOpen}
                onOpenChange={setIsConfigOpen}
                initData={initData}
                onSubmit={handleConfigSubmit}
                onCancel={handleConfigCancel}
                adaptiveQualityEnabled={adaptiveQualityEnabled}
                onAdaptiveQualityChange={setAdaptiveQualityEnabled}
                adaptiveBitrateEnabled={adaptiveBitrateEnabled}
                onAdaptiveBitrateChange={setAdaptiveBitrateEnabled}
            />
            <div className="flex items-center justify-between border-b p-4">
                <h2 className="text-lg font-semibold">{t('pages.desk.title')} - {deskId}</h2>
                <div className="flex items-center gap-4">
                    {hasControl && (
                        <div className="text-sm font-medium text-blue-500">{t('pages.desk.status.controlling')}</div>
                    )}
                    {isConnected && (
                        <ConnectionQualityBadge packetLoss={rtcStats.packetLoss} rtt={rtcStats.rtt} />
                    )}
                    <div className="flex items-center gap-2">
                        <div className={`h-2 w-2 rounded-full ${isConnected ? 'bg-green-500' : 'bg-red-500'}`} />
                        <span className="text-sm text-muted-foreground">{isConnected ? t('pages.desk.status.connected') : t('pages.desk.status.disconnected')}</span>
                    </div>
                </div>
            </div>
            <div className="relative flex-1 bg-black flex items-center justify-center overflow-hidden">
                {!isConnected && (
                    <div className="flex flex-col items-center gap-2 text-white z-50">
                        <Loader2 className="h-8 w-8 animate-spin" />
                        <span>{t('pages.desk.status.connecting')}</span>
                    </div>
                )}

                <TooltipProvider>
                    <div
                        className="videoWrapper"
                        ref={videoWrapperRef}
                    >
                        <video
                            ref={videoRef}
                            className="videoElement h-full w-full object-contain"
                            style={{ cursor: cursorStyle }}
                            autoPlay
                            playsInline
                            muted={isMuted}
                            tabIndex={0}
                            onCanPlay={() => setIsVideoReady(true)}
                        />

                        {/* Escape hint shown in fullscreen when the Keyboard
                            Lock API can't capture Esc. Lives inside the
                            fullscreen element so it stays in the top layer. */}
                        {showEscHint && (
                            <div className="absolute top-5 left-1/2 -translate-x-1/2 z-[60] flex max-w-[92%] items-center gap-3 rounded-xl border border-amber-300/70 bg-amber-500/95 px-5 py-3 text-sm font-semibold text-amber-950 shadow-2xl shadow-black/40 ring-1 ring-black/20 backdrop-blur-md animate-in fade-in slide-in-from-top-4 zoom-in-95 duration-300">
                                <AlertTriangle className="h-6 w-6 shrink-0 animate-pulse" />
                                <span className="leading-snug">
                                    {t('pages.desk.escHintFullscreen')}
                                </span>
                            </div>
                        )}

                        {/* Whiteboard canvas overlay */}
                        <WhiteboardCanvas
                            elements={whiteboard.elements}
                            isActive={whiteboard.isActive}
                            videoRef={videoRef}
                            onPointerDown={whiteboard.handlePointerDown}
                            onPointerMove={whiteboard.handlePointerMove}
                            onPointerUp={whiteboard.handlePointerUp}
                            onClick={whiteboard.handleCanvasClick}
                        />

                        {/* Whiteboard toolbar */}
                        {whiteboard.isActive && (
                            <WhiteboardToolbar
                                tool={whiteboard.tool}
                                setTool={whiteboard.setTool}
                                color={whiteboard.color}
                                setColor={whiteboard.setColor}
                                strokeWidth={whiteboard.strokeWidth}
                                setStrokeWidth={whiteboard.setStrokeWidth}
                                onClear={whiteboard.clearAll}
                                onUndo={whiteboard.undo}
                                onClose={whiteboard.toggleWhiteboard}
                            />
                        )}

                        {/* Whiteboard text input overlay */}
                        {whiteboard.textInput && (
                            <form
                                className="fixed z-[9999] flex items-center gap-2 bg-background p-2 rounded-lg shadow-lg border border-white/20"
                                style={{
                                    left: Math.min(whiteboard.textInput.clientX, window.innerWidth - 300),
                                    top: Math.min(whiteboard.textInput.clientY, window.innerHeight - 60)
                                }}
                                onSubmit={(e) => {
                                    e.preventDefault();
                                    const input = e.currentTarget.elements.namedItem('textInput') as HTMLInputElement;
                                    whiteboard.confirmTextInput(input.value);
                                }}
                            >
                                <Input
                                    id="textInput"
                                    name="textInput"
                                    autoFocus
                                    className="w-48 h-8 text-foreground"
                                    placeholder={t('pages.desk.enterText')}
                                    onKeyDown={(e) => {
                                        if (e.key === 'Enter') {
                                            if (e.currentTarget.value.trim()) {
                                                whiteboard.confirmTextInput(e.currentTarget.value);
                                            } else {
                                                whiteboard.cancelTextInput();
                                            }
                                        }
                                        if (e.key === 'Escape') {
                                            whiteboard.cancelTextInput();
                                        }
                                    }}
                                /*
                                onBlur={(e) => {
                                    // Auto-confirm on blur if not empty, otherwise cancel
                                    if (e.target.value.trim()) {
                                        whiteboard.confirmTextInput(e.target.value);
                                    } else {
                                        whiteboard.cancelTextInput();
                                    }
                                }}
                                */
                                />
                            </form>
                        )}

                        <div
                            className={`videoPlaceholder ${isVideoReady ? 'hidden' : ''}`}
                            onContextMenu={(e) => { e.preventDefault() }}
                        >
                            <div className="placeholderContent">
                                <span className="artText">LCXL Remote Desk</span>
                                {!initData && (
                                    <p className="mt-3 text-sm text-white/80">
                                        {t('pages.desk.initializingCapture')}
                                        <br />
                                        {t('pages.desk.waitingPermission')}
                                    </p>
                                )}
                            </div>
                        </div>

                        {showStats && isConnected && (
                            <DeskSessionStats
                                adaptiveQualityEnabled={adaptiveQualityEnabled}
                                currentVideoQuality={lastSettingsRef.current?.video_quality ?? null}
                                lastQualityAdjustedAt={lastQualityAdjustRef.current}
                                onClose={() => setShowStats(false)}
                                qualityAdjustmentCount={qualityAdjustmentCountRef.current}
                                rtcStats={rtcStats}
                            />
                        )}

                        {/* The desktop-switching / reconnecting /
                         * reconnect-timeout overlays are gone because the
                         * daemon-side keep-PC swap path means worker swaps
                         * no longer tear down the browser PC, so there is
                         * nothing to overlay. */}

                        {isWaitingApproval && (
                            <div className="absolute top-1/2 left-1/2 transform -translate-x-1/2 -translate-y-1/2 z-50 bg-black/80 text-white px-6 py-4 rounded-lg shadow-2xl backdrop-blur-md border border-white/10 flex flex-col items-center gap-4 animate-in fade-in slide-in-from-bottom-4">
                                <Loader2 className="w-8 h-8 animate-spin text-blue-400" />
                                <span className="text-lg font-medium">{t('pages.desk.waitingPermission')}</span>
                            </div>
                        )}

                        {errorMessage && (
                            <div className="absolute top-16 right-4 z-[60] bg-red-500/90 text-white px-4 py-2 rounded-lg text-sm font-medium shadow-lg backdrop-blur-md animate-in fade-in slide-in-from-top-4">
                                {errorMessage}
                            </div>
                        )}

                        {showDiagnose && (
                            <DiagnosePanel
                                state={diagnose.state}
                                onStart={diagnose.start}
                                onReset={diagnose.reset}
                                onClose={() => setShowDiagnose(false)}
                                isConnected={isConnected}
                                exec={exec}
                                onApproveExec={diagnose.approveExec}
                                onRejectExec={diagnose.rejectExec}
                                onCancelBackgroundExec={diagnose.cancelBackgroundExec}
                                historySessions={diagnose.historySessions}
                                historyLoading={diagnose.historyLoading}
                                historyError={diagnose.historyError}
                                onRefreshHistory={diagnose.refreshHistory}
                                onRestoreSession={diagnose.restoreSession}
                                canContinue={diagnose.canContinue}
                                orgId={orgId}
                            />
                        )}

                        {transferStatus !== 'idle' && transferStatus !== 'error' && (
                            <div className="absolute top-16 right-4 z-[60] bg-black/80 text-white px-4 py-2 rounded-lg text-sm font-medium shadow-lg backdrop-blur-md border border-white/10 flex items-center gap-2 animate-in fade-in slide-in-from-top-4">
                                <Loader2 className="w-4 h-4 animate-spin text-blue-400" />
                                <span>{transferProgress ? `${t('pages.desk.syncing')} ${transferProgress}%` : t('pages.desk.syncing')}</span>
                            </div>
                        )}

                        {fallbackToast.show && (
                            <ClipboardFallbackToast
                                onClose={closeFallbackToast}
                                onSync={execFallbackToastAction}
                                text={fallbackToast.text}
                            />
                        )}

                        {resolutionToast && (
                            <ResolutionStatusToast toast={resolutionToast} />
                        )}

                        {isConnected && (
                            <DeskControlBar
                                audioVolume={audioVolume}
                                clipboardEnabled={clipboardEnabled}
                                controlBarRef={controlBarRef}
                                hasControl={hasControl}
                                isDragging={isDragging}
                                isFullscreen={isFullscreen}
                                isMuted={isMuted}
                                isPrivateScreen={isPrivateScreen}
                                isPrivateScreenPending={isPrivateScreenPending}
                                isPrivateScreenSupported={isPrivateScreenSupported}
                                isWaitingApproval={isWaitingApproval}
                                keyboardLockSupported={keyboardLockSupported}
                                microphone={microphone}
                                onChangeVolume={handleVolumeChange}
                                onDisconnect={handleDisconnect}
                                onDragStart={handleDragStart}
                                onOpenSettings={() => setIsConfigOpen(true)}
                                onRequestControl={handleRequestControl}
                                onSendKeyboardEvents={sendKeyboardEvents}
                                onToggleClipboard={toggleClipboard}
                                onToggleFullscreen={handleFullscreen}
                                onTogglePrivateScreen={handleTogglePrivateScreen}
                                operationSystem={initData?.operation_system}
                                restricted={restricted}
                                setShowDiagnose={setShowDiagnose}
                                setShowStats={setShowStats}
                                showDiagnose={showDiagnose}
                                showStats={showStats}
                                whiteboard={whiteboard}
                            />
                        )}
                    </div>
                </TooltipProvider>
            </div >
        </div >
    )
}
