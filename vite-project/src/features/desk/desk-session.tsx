import { useEffect, useRef, useState, useCallback } from "react"
import type { MouseEvent as ReactMouseEvent } from "react"
import { useParams, useNavigate } from "react-router-dom"
import { useTranslation } from "react-i18next"
import { Menu, Loader2, Folder, Terminal as TerminalIcon, MousePointer2, XSquare, Maximize, Minimize, Settings, Volume2, VolumeX, Power, Keyboard, Activity, ShieldCheck, ShieldOff, Clipboard, ClipboardX, PenTool, Mic, MicOff, CheckCircle2, AlertCircle, Sparkles } from "lucide-react"
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip"
import {
    DropdownMenu,
    DropdownMenuContent,
    DropdownMenuItem,
    DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu"
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover"
import { Slider } from "@/components/ui/slider"

import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import "./desk-session.css"
import { useDeskSignaling } from "./use-desk-signaling"
import { useDeskRTC } from "./use-desk-rtc"
import { useDeskDiagnose } from "./use-desk-diagnose"
import { DiagnosePanel } from "./diagnose-panel"
import { useDeskExec } from "./use-desk-exec"
import { useDeskInput } from "./use-desk-input"
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
} from "./constants"

/**
 * Whether the desk config dialog should be (re)opened automatically.
 *
 * It opens for the initial settings pick (INIT arrived, no connect attempt yet)
 * and after a terminal ICE failure so the user can retry. It must NOT reopen on
 * a transient `disconnected` — that flips `isRTCConnected` to false but leaves
 * `rtcFailed` false, and ICE typically recovers on its own; reopening there
 * leaves the recovered video sitting behind a spurious dialog (the reported
 * "dialog pops back up, then the screen appears behind it" flapping). Pure so
 * the unit test can pin every branch without rendering the component.
 */
export function shouldOpenConfigDialog(args: {
    hasInitData: boolean
    isRTCConnected: boolean
    hasAttemptedConnect: boolean
    rtcFailed: boolean
}): boolean {
    const { hasInitData, isRTCConnected, hasAttemptedConnect, rtcFailed } = args
    if (!hasInitData || isRTCConnected) {
        return false
    }
    return !hasAttemptedConnect || rtcFailed
}

export default function DeskSession() {
    const { id: deskId } = useParams<{ id: string }>()
    const navigate = useNavigate()
    const { t } = useTranslation()
    const { toast } = useToast()

    // Control state
    const [hasControl, setHasControl] = useState(false);
    const [hasRequested, setHasRequested] = useState(false);
    const [isWaitingApproval, setIsWaitingApproval] = useState(false);
    const hasRequestedRef = useRef(false);

    const { isConnected, lastMessage, sendMessage } = useDeskSignaling(deskId || null)

    const handleConnect = useCallback(() => {
        if (deskId && !hasRequestedRef.current) {
            console.log("WebSocket opened, requesting remote connection directly:", deskId);
            sendMessage(SIGNALING_TYPE_CODE_REQUEST_REMOTE, { connection_id: deskId }, deskId);
            hasRequestedRef.current = true;
            setHasRequested(true);
        }
    }, [deskId, sendMessage]);

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

    // Drag UI state
    const [isDragging, setIsDragging] = useState(false);
    const [dragOffset, setDragOffset] = useState({ x: 0, y: 0 });

    const [showStats, setShowStats] = useState(false);
    const [showDiagnose, setShowDiagnose] = useState(false);
    const [isDiagnoseHovered, setIsDiagnoseHovered] = useState(false);

    const [isControlBarHovered, setIsControlBarHovered] = useState(false);
    const [isControlBarMenuOpen, setIsControlBarMenuOpen] = useState(false);
    const isControlBarExpanded = isControlBarHovered || isControlBarMenuOpen || isDragging;

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

    const { peerConnection, remoteStream, initData, connect, mouseChannel, keyboardChannel, mouseMoveChannel, clipboardChannel, whiteboardChannel, cursorSyncChannel, isRTCConnected, rtcFailed, closeRTC, rtcStats } = useDeskRTC({
        deskId: deskId || null,
        lastMessage,
        sendMessage
    });

    // AI diagnose stream: sends `Diagnose` and aggregates `DiagnoseEvent`
    // frames off the same signaling channel.
    const diagnose = useDeskDiagnose({
        deskId: deskId || null,
        lastMessage,
        sendMessage,
    });

    // Confirmed execution of a suggested command: ConfirmExec -> ExecPreview ->
    // ResolveExec -> ExecResult, keyed by command row.
    const exec = useDeskExec({
        deskId: deskId || null,
        lastMessage,
        sendMessage,
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
    // seen an echo for. The lastMessage listener uses this set as a
    // membership check to detect auto-resolution echoes (so manual
    // path echoes from future UI keep working unchanged), while the
    // `useResolutionToast` hook below drives the right-bottom toast
    // state machine off the same `lastMessage` stream.
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
            lastMessage,
            isRTCConnected,
            changeDisplaySettingsType: SIGNALING_TYPE_CODE_CHANGE_DISPLAY_SETTINGS,
            translate: (key, fallback) => t(key, fallback),
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

    // Handle incoming signaling messages regarding control
    useEffect(() => {
        if (!lastMessage) return;
        const { signaling_type } = lastMessage;

        if (signaling_type === SIGNALING_TYPE_CODE_ACCEPT_CONTROL) {
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
            const data = lastMessage.signaling_data;
            if (data) {
                console.log("Private screen state changed:", data);
                setIsPrivateScreen(data.visible ?? false);
                setIsPrivateScreenSupported(data.is_supported ?? true);
                if (data.error_msg) {
                    console.error("Private screen error:", data.error_msg);
                }
            }
        } else if (signaling_type === SIGNALING_TYPE_CODE_AUDIO_PLAYBACK_ERROR) {
            const data = lastMessage.signaling_data;
            if (data && data.error) {
                console.error("Remote audio playback error:", data.error);
                forceError(data.error);
            }
        } else if (signaling_type === SIGNALING_TYPE_CODE_CHANGE_DISPLAY_SETTINGS) {
            // Adaptive-resolution echo: the right-bottom status toast
            // is driven by `useResolutionToast`, which subscribes to
            // `lastMessage` directly and gates transitions by the most
            // recent request id. Here we only need to drain
            // `pendingAutoRequestIdsRef` so the membership-tracking
            // contract used by future manual ChangeDisplaySettings UI
            // does not leak.
            const requestId = lastMessage.request_id;
            if (requestId && pendingAutoRequestIdsRef.current.delete(requestId)) {
                console.debug("[adaptive-resolution] response", lastMessage);
            }
        }
    }, [lastMessage, forceError, sendMessage, deskId]);

    // Reset requested state if connection drops
    useEffect(() => {
        if (!isConnected) {
            hasRequestedRef.current = false;
            setHasRequested(false);
        }
    }, [isConnected]);

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
    // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [rtcStats, adaptiveQualityEnabled]);

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
                title: t("pages.desk.webrtcUnavailableTitle", "WebRTC unavailable"),
                description: t(
                    "pages.desk.webrtcUnavailableDesc",
                    "This client's built-in browser does not support WebRTC. Please open the web console in a standard browser such as Chrome to connect.",
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
    // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [adaptiveBitrateEnabled]);

    const handleConfigCancel = () => {
        setIsConfigOpen(false);
        navigate(`/desk/${deskId}`);
    };

    const handleRequestControl = () => {
        const requestControlData = {
            accept: !hasControl,
            accept_clipboard_sync: !hasControl, // Auto-request clipboard by default on control
            accept_file_transfer: !hasControl,
        };
        // Auto-enable UI state if asking for control
        if (!hasControl && window.isSecureContext !== false) {
            if (!clipboardEnabled) toggleClipboard();
        }

        console.log(`Sending REQUIRE_CONTROL signaling, requestControlData:`, requestControlData);
        sendMessage(SIGNALING_TYPE_CODE_REQUIRE_CONTROL, requestControlData, deskId);
        setIsWaitingApproval(true);
    };

    const handleDisconnect = () => {
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
        console.log(`Toggling private screen: ${newState}`);
        sendMessage(SIGNALING_TYPE_CODE_ENABLE_PRIVATE_SCREEN, { enable: newState }, deskId);
    };

    const handleFullscreen = () => {
        if (!document.fullscreenElement) {
            videoWrapperRef.current?.requestFullscreen().catch(err => {
                console.log(`Error attempting to enable fullscreen: ${err.message}`);
            });
            try {
                (navigator as any).keyboard?.lock(['Escape']);
                console.log("Keyboard lock: ESC key captured");
            } catch (error) {
                console.warn("Failed to lock keyboard:", error);
            }
        } else {
            document.exitFullscreen();
            try {
                (navigator as any).keyboard?.unlock();
                console.log("Keyboard unlock: ESC key released");
            } catch (error) {
                console.warn("Failed to unlock keyboard:", error);
            }
        }
    };

    // Keep fullscreen state in sync
    useEffect(() => {
        const handleFullscreenChange = () => {
            setIsFullscreen(!!document.fullscreenElement);
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

    const handleVolumeChange = (value: number) => {
        if (!videoRef.current) return;
        setAudioVolume(value);
        videoRef.current.volume = value / 100;
        setIsMuted(value === 0);
    };

    const toggleMute = () => {
        if (videoRef.current) {
            const newMuted = !isMuted;
            videoRef.current.muted = newMuted;
            setIsMuted(newMuted);
        }
    };

    // Drag handlers for control bar
    const handleDragStart = (e: ReactMouseEvent) => {
        if (!controlBarRef.current || !videoWrapperRef.current) return;

        setIsDragging(true);
        const controlBar = controlBarRef.current;
        const wrapperRect = videoWrapperRef.current.getBoundingClientRect();
        const controlBarRect = controlBar.getBoundingClientRect();

        setDragOffset({
            x: e.clientX - controlBarRect.left,
            y: e.clientY - controlBarRect.top
        });

        // Disable transition while dragging
        controlBar.style.transition = 'none';
        controlBar.style.transform = 'none';
        controlBar.style.bottom = 'auto';

        controlBar.style.left = `${controlBarRect.left - wrapperRect.left}px`;
        controlBar.style.top = `${controlBarRect.top - wrapperRect.top}px`;
    };

    const handleDrag = useCallback((e: MouseEvent) => {
        if (!isDragging || !controlBarRef.current || !videoWrapperRef.current) return;
        e.preventDefault();

        const controlBar = controlBarRef.current;
        const wrapperRect = videoWrapperRef.current.getBoundingClientRect();

        const screenX = e.clientX - dragOffset.x;
        const screenY = e.clientY - dragOffset.y;

        let newX = screenX - wrapperRect.left;
        let newY = screenY - wrapperRect.top;

        const maxX = wrapperRect.width - controlBar.offsetWidth;
        const maxY = wrapperRect.height - controlBar.offsetHeight;

        newX = Math.max(0, Math.min(newX, maxX));
        newY = Math.max(0, Math.min(newY, maxY));

        controlBar.style.left = `${newX}px`;
        controlBar.style.top = `${newY}px`;
    }, [isDragging, dragOffset]);

    const handleDragEnd = useCallback(() => {
        setIsDragging(false);
        if (controlBarRef.current) {
            controlBarRef.current.style.transition = '';
        }
    }, []);

    useEffect(() => {
        if (isDragging) {
            document.addEventListener('mousemove', handleDrag);
            document.addEventListener('mouseup', handleDragEnd);
        } else {
            document.removeEventListener('mousemove', handleDrag);
            document.removeEventListener('mouseup', handleDragEnd);
        }
        return () => {
            document.removeEventListener('mousemove', handleDrag);
            document.removeEventListener('mouseup', handleDragEnd);
        };
    }, [isDragging, handleDrag, handleDragEnd]);

    // Handle generic window resizing to clamp control bar position
    useEffect(() => {
        const wrapper = videoWrapperRef.current;
        if (!wrapper) return;

        const resizeObserver = new ResizeObserver(() => {
            const controlBar = controlBarRef.current;
            if (!controlBar || controlBar.style.transform !== 'none') return; // Only process if previously dragged

            const wrapperRect = wrapper.getBoundingClientRect();

            // Current set inline style properties (numeric limits)
            const currentLeft = parseFloat(controlBar.style.left) || 0;
            const currentTop = parseFloat(controlBar.style.top) || 0;

            const maxX = wrapperRect.width - controlBar.offsetWidth;
            const maxY = wrapperRect.height - controlBar.offsetHeight;

            // Clamp and adjust
            const newX = Math.max(0, Math.min(currentLeft, maxX));
            const newY = Math.max(0, Math.min(currentTop, maxY));

            controlBar.style.left = `${newX}px`;
            controlBar.style.top = `${newY}px`;
        });

        resizeObserver.observe(wrapper);
        return () => resizeObserver.disconnect();
    }, []);

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
                <h2 className="text-lg font-semibold">{t('pages.desk.title', 'Remote Desk')} - {deskId}</h2>
                <div className="flex items-center gap-4">
                    {hasControl && (
                        <div className="text-sm font-medium text-blue-500">Controlling</div>
                    )}
                    <div className="flex items-center gap-2">
                        <div className={`h-2 w-2 rounded-full ${isConnected ? 'bg-green-500' : 'bg-red-500'}`} />
                        <span className="text-sm text-muted-foreground">{isConnected ? 'Connected' : 'Disconnected'}</span>
                    </div>
                </div>
            </div>
            <div className="relative flex-1 bg-black flex items-center justify-center overflow-hidden">
                {!isConnected && (
                    <div className="flex flex-col items-center gap-2 text-white z-50">
                        <Loader2 className="h-8 w-8 animate-spin" />
                        <span>Connecting...</span>
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

                        {/* Whiteboard canvas overlay */}
                        <WhiteboardCanvas
                            elements={whiteboard.elements}
                            isActive={whiteboard.isActive}
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
                                    placeholder={t('pages.desk.enterText', 'Enter text...')}
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
                                        {t('pages.desk.initializingCapture', 'Initializing remote capture...')}
                                        <br />
                                        {t('pages.desk.waitingPermission', 'Waiting for host authorization dialog.')}
                                    </p>
                                )}
                            </div>
                        </div>

                        {showStats && isConnected && (
                            <div className="absolute top-4 left-4 z-50 bg-black/60 text-white p-3 rounded-lg text-xs font-mono backdrop-blur-md border border-white/20 select-none min-w-[260px] max-h-[80vh] overflow-y-auto">
                                <div className="flex justify-between items-center mb-2 pb-1 border-b border-white/15">
                                    <div className="text-sm font-bold text-white/90">
                                        {t('pages.desk.statsPanel.title', 'Remote Desk Metrics')}
                                    </div>
                                    <button 
                                        onClick={() => setShowStats(false)} 
                                        className="text-gray-400 hover:text-white transition-colors"
                                        aria-label={t('pages.desk.closeStats', 'Close')}
                                    >
                                        <XSquare className="w-4 h-4" />
                                    </button>
                                </div>

                                {/* Network section — what was the original "Network Stats" panel. */}
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">{t('pages.desk.statsPanel.fps', 'FPS')}:</span>
                                    <span className="font-bold text-green-400">{rtcStats.fps}</span>
                                </div>
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">{t('pages.desk.statsPanel.resolution', 'Resolution')}:</span>
                                    <span className="font-bold text-white">{rtcStats.width}x{rtcStats.height}</span>
                                </div>
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">{t('pages.desk.statsPanel.bitrate', 'Bitrate')}:</span>
                                    <span className="font-bold text-blue-400">{rtcStats.bitrate} kbps</span>
                                </div>
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">{t('pages.desk.statsPanel.videoCodec', 'Video Codec')}:</span>
                                    <span className="font-bold text-purple-400">{rtcStats.videoCodec || 'Unknown'}</span>
                                </div>
                                {rtcStats.audioCodec && (
                                    <div className="flex justify-between gap-4 mb-1">
                                        <span className="text-gray-400">{t('pages.desk.statsPanel.audioCodec', 'Audio Codec')}:</span>
                                        <span className="font-bold text-purple-400">{rtcStats.audioCodec}</span>
                                    </div>
                                )}
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">{t('pages.desk.statsPanel.latency', 'Latency')}:</span>
                                    <span className={`font-bold ${rtcStats.rtt > 150 ? 'text-red-400' : rtcStats.rtt > 80 ? 'text-yellow-400' : 'text-green-400'}`}>
                                        {rtcStats.rtt} ms
                                    </span>
                                </div>
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">{t('pages.desk.statsPanel.packetLoss', 'Packet Loss')}:</span>
                                    <span className={`font-bold ${rtcStats.packetLoss > 5 ? 'text-red-400' : rtcStats.packetLoss > 1 ? 'text-yellow-400' : 'text-green-400'}`}>
                                        {rtcStats.packetLoss}%
                                    </span>
                                </div>
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">{t('pages.desk.statsPanel.network', 'Network')}:</span>
                                    <span className="font-bold text-orange-400 uppercase">
                                        {rtcStats.networkType || 'Unknown'}
                                    </span>
                                </div>

                                {/* Video frame section — derived from RTCInboundRtpStreamStats. */}
                                <div className="text-xs font-bold text-white/80 mt-3 mb-1 pt-2 border-t border-white/15">
                                    {t('pages.desk.statsPanel.frameSection', 'Video Frames')}
                                </div>
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">{t('pages.desk.statsPanel.framesDecoded', 'Frames Decoded')}:</span>
                                    <span className="font-bold text-white">{rtcStats.framesDecoded} (+{rtcStats.framesDecodedDelta})</span>
                                </div>
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">{t('pages.desk.statsPanel.keyFrames', 'I Frames')}:</span>
                                    {/* High keyframe rate = encoder churn / frequent PLI / wasted bandwidth.
                                        Highlight when more than ~1 IDR/s (the symptom we just hunted in the broadcast-lag bug). */}
                                    <span className={`font-bold ${rtcStats.keyFramesDelta > 1 ? 'text-red-400' : 'text-yellow-300'}`}>
                                        {rtcStats.keyFramesDecoded} (+{rtcStats.keyFramesDelta})
                                    </span>
                                </div>
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">{t('pages.desk.statsPanel.pFrames', 'P Frames')}:</span>
                                    <span className="font-bold text-white">{rtcStats.pFramesDecoded}</span>
                                </div>
                                {/* webrtc-rs 0.17.x's VP9 RTP packetizer hard-codes the
                                    payload-header byte to 0x90 and never sets the P bit
                                    (`rtp-0.17.1/src/codecs/vp9/mod.rs:110`). The browser
                                    therefore reads every VP9 packet as a keyframe, so
                                    `keyFramesDecoded ≈ framesDecoded` regardless of the
                                    encoder's actual GOP. The encoder side (server logs,
                                    `MediaFrameKind::VideoI` counters) is unaffected, and
                                    bytes/bitrate stats reflect real payload size. Surface
                                    this only when VP9 is actually negotiated to avoid
                                    confusing operators on other codecs. */}
                                {rtcStats.videoCodec === 'VP9' && (
                                    <div className="text-[10px] italic text-gray-500 mt-0.5 leading-tight">
                                        {t('pages.desk.statsPanel.vp9FrameTypeHint', 'VP9 RTP packetizer does not tag P frames; the browser counts every frame as an I frame. Encoder-side GOP is unaffected.')}
                                    </div>
                                )}
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">{t('pages.desk.statsPanel.framesDropped', 'Frames Dropped')}:</span>
                                    <span className={`font-bold ${rtcStats.framesDropped > 0 ? 'text-yellow-300' : 'text-white'}`}>{rtcStats.framesDropped}</span>
                                </div>
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">{t('pages.desk.statsPanel.avgQp', 'Avg QP')}:</span>
                                    {/* `null` means the browser doesn't report `qpSum` for this
                                        codec/decoder path. Common on Chromium with H.264
                                        hardware decoding (NVDEC / QuickSync) — not a bug, just
                                        a metric that isn't available. */}
                                    <span className={`font-bold ${rtcStats.avgQp === null ? 'text-gray-500 italic' : 'text-white'}`}>
                                        {rtcStats.avgQp === null
                                            ? t('pages.desk.statsPanel.avgQpUnavailable', 'N/A (hw decode)')
                                            : rtcStats.avgQp}
                                    </span>
                                </div>

                                {/* RTCP feedback — receiver-initiated requests for keyframes / NACK / FIR. */}
                                <div className="text-xs font-bold text-white/80 mt-3 mb-1 pt-2 border-t border-white/15">
                                    {t('pages.desk.statsPanel.feedbackSection', 'RTCP Feedback')}
                                </div>
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">{t('pages.desk.statsPanel.pliCount', 'PLI Sent')}:</span>
                                    {/* PLI rate > 0 every sample window means the browser is repeatedly
                                        asking for keyframes — e.g. heavy packet loss or decoder reset. */}
                                    <span className={`font-bold ${rtcStats.pliDelta > 0 ? 'text-red-400' : 'text-white'}`}>
                                        {rtcStats.pliCount} (+{rtcStats.pliDelta})
                                    </span>
                                </div>
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">{t('pages.desk.statsPanel.nackCount', 'NACK Sent')}:</span>
                                    <span className="font-bold text-white">{rtcStats.nackCount}</span>
                                </div>
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">{t('pages.desk.statsPanel.firCount', 'FIR Sent')}:</span>
                                    <span className="font-bold text-white">{rtcStats.firCount}</span>
                                </div>

                                {/* Encoder quality — sender-side knob the adaptive loop drives.
                                    The headline question this section answers is "is adaptive
                                    quality oscillating?" Read straight off the refs because the
                                    enclosing component already re-renders every second when
                                    `rtcStats` updates, so the values are fresh without an
                                    extra setState. */}
                                <div className="text-xs font-bold text-white/80 mt-3 mb-1 pt-2 border-t border-white/15">
                                    {t('pages.desk.statsPanel.encoderSection', 'Encoder Quality')}
                                </div>
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">{t('pages.desk.statsPanel.currentQuality', 'Current Quality')}:</span>
                                    {/* video_quality is QP-style: 0-63, lower is sharper.
                                        Mirror the same colour buckets as Avg QP so the two are visually comparable. */}
                                    <span className={`font-bold ${
                                        (lastSettingsRef.current?.video_quality ?? 22) > 40
                                            ? 'text-red-400'
                                            : (lastSettingsRef.current?.video_quality ?? 22) > 30
                                                ? 'text-yellow-300'
                                                : 'text-green-400'
                                    }`}>
                                        {lastSettingsRef.current?.video_quality ?? '-'}
                                    </span>
                                </div>
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">{t('pages.desk.statsPanel.qualityAdjustments', 'Adaptive Adjustments')}:</span>
                                    {/* If the count grows by more than ~1/min the controller is
                                        thrashing. Highlight when adaptive is even on so the user
                                        can tell at a glance whether the toggle has any effect. */}
                                    <span className={`font-bold ${
                                        adaptiveQualityEnabled
                                            ? qualityAdjustmentCountRef.current > 5 ? 'text-yellow-300' : 'text-white'
                                            : 'text-gray-500'
                                    }`}>
                                        {qualityAdjustmentCountRef.current}
                                        {!adaptiveQualityEnabled && ' (off)'}
                                    </span>
                                </div>
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">{t('pages.desk.statsPanel.lastAdjustedAgo', 'Last Adjusted')}:</span>
                                    <span className="font-bold text-white">
                                        {qualityAdjustmentCountRef.current === 0
                                            ? '-'
                                            : `${Math.max(0, Math.round((Date.now() - lastQualityAdjustRef.current) / 1000))} s`}
                                    </span>
                                </div>

                                {/* Playback quality — perceived smoothness signals. */}
                                <div className="text-xs font-bold text-white/80 mt-3 mb-1 pt-2 border-t border-white/15">
                                    {t('pages.desk.statsPanel.qualitySection', 'Playback Quality')}
                                </div>
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">{t('pages.desk.statsPanel.freezeCount', 'Freeze Count')}:</span>
                                    <span className={`font-bold ${rtcStats.freezeCount > 0 ? 'text-yellow-300' : 'text-white'}`}>{rtcStats.freezeCount}</span>
                                </div>
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">{t('pages.desk.statsPanel.totalFreezeDuration', 'Total Freeze Duration')}:</span>
                                    <span className="font-bold text-white">{rtcStats.totalFreezesDurationMs} ms</span>
                                </div>
                                <div className="flex justify-between gap-4">
                                    <span className="text-gray-400">{t('pages.desk.statsPanel.jitter', 'Jitter')}:</span>
                                    <span className={`font-bold ${rtcStats.jitterMs > 50 ? 'text-yellow-300' : 'text-white'}`}>{rtcStats.jitterMs} ms</span>
                                </div>
                                {/* RFC 3550 interarrival jitter is an EWMA over packet interarrival
                                    deltas. Worker drops to ~1 fps on a static desktop, and at that
                                    packet rate any single OS scheduling blip dominates the EWMA, so
                                    the value drifts up to hundreds of ms. Once the screen moves and
                                    fps recovers, the EWMA snaps back within a couple of seconds.
                                    Surface this so operators don't mistake it for a real network
                                    fault. */}
                                <div className="text-[10px] italic text-gray-500 mt-0.5 leading-tight">
                                    {t('pages.desk.statsPanel.jitterHint', 'Static desktop drops to ~1 fps; RFC 3550 jitter is unreliable at low packet rates')}
                                </div>
                            </div>
                        )}

                        {/* The desktop-switching / reconnecting /
                         * reconnect-timeout overlays are gone because the
                         * daemon-side keep-PC swap path means worker swaps
                         * no longer tear down the browser PC, so there is
                         * nothing to overlay. */}

                        {isWaitingApproval && (
                            <div className="absolute top-1/2 left-1/2 transform -translate-x-1/2 -translate-y-1/2 z-50 bg-black/80 text-white px-6 py-4 rounded-lg shadow-2xl backdrop-blur-md border border-white/10 flex flex-col items-center gap-4 animate-in fade-in slide-in-from-bottom-4">
                                <Loader2 className="w-8 h-8 animate-spin text-blue-400" />
                                <span className="text-lg font-medium">{t('pages.desk.waitingPermission', 'Waiting for host authorization dialog.')}</span>
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
                                onHandoff={diagnose.handoff}
                                onReset={diagnose.reset}
                                onClose={() => setShowDiagnose(false)}
                                isConnected={isConnected}
                                exec={exec}
                                onApproveExec={diagnose.approveExec}
                                onRejectExec={diagnose.rejectExec}
                            />
                        )}

                        {transferStatus !== 'idle' && transferStatus !== 'error' && (
                            <div className="absolute top-16 right-4 z-[60] bg-black/80 text-white px-4 py-2 rounded-lg text-sm font-medium shadow-lg backdrop-blur-md border border-white/10 flex items-center gap-2 animate-in fade-in slide-in-from-top-4">
                                <Loader2 className="w-4 h-4 animate-spin text-blue-400" />
                                <span>{transferProgress ? `${t('pages.desk.syncing', 'Syncing clipboard...')} ${transferProgress}%` : t('pages.desk.syncing', 'Syncing clipboard...')}</span>
                            </div>
                        )}

                        {fallbackToast.show && (
                            <div className="absolute bottom-24 right-4 z-[60] bg-amber-500/90 text-white p-4 rounded-lg shadow-xl flex flex-col gap-3 min-w-[300px] animate-in slide-in-from-bottom-4 pointer-events-auto">
                                <div className="flex justify-between items-start">
                                    <span className="font-semibold text-sm">Action Required</span>
                                    <button onClick={closeFallbackToast} className="text-white/80 hover:text-white"><XSquare className="w-4 h-4" /></button>
                                </div>
                                <p className="text-xs text-amber-100">{fallbackToast.text || 'Clipboard update received, please click to sync.'}</p>
                                <Button variant="secondary" size="sm" className="w-full bg-white text-amber-900 hover:bg-amber-100" onClick={execFallbackToastAction}>
                                    Sync Now
                                </Button>
                            </div>
                        )}

                        {resolutionToast && (
                            <div
                                data-testid="resolution-toast"
                                data-phase={resolutionToast.phase}
                                className={`absolute bottom-40 right-4 z-[60] px-3 py-2 rounded-lg shadow-lg text-xs font-medium backdrop-blur-md border flex items-center gap-2 animate-in slide-in-from-bottom-4 pointer-events-none ${
                                    resolutionToast.phase === "success"
                                        ? "bg-emerald-500/90 text-white border-emerald-300/40"
                                        : resolutionToast.phase === "failed"
                                            ? "bg-red-500/90 text-white border-red-300/40"
                                            : "bg-black/80 text-white border-white/10"
                                }`}
                            >
                                {resolutionToast.phase === "updating" && (
                                    <>
                                        <Loader2 className="w-4 h-4 animate-spin text-blue-300" />
                                        <span>
                                            {t(
                                                "pages.desk.resolutionUpdating",
                                                "Updating resolution {{w}}×{{h}}…",
                                                {
                                                    w: resolutionToast.targetW,
                                                    h: resolutionToast.targetH,
                                                },
                                            )}
                                        </span>
                                    </>
                                )}
                                {resolutionToast.phase === "success" && (
                                    <>
                                        <CheckCircle2 className="w-4 h-4" />
                                        <span>
                                            {t(
                                                "pages.desk.resolutionApplied",
                                                "Applied {{w}}×{{h}}",
                                                {
                                                    w: resolutionToast.appliedW,
                                                    h: resolutionToast.appliedH,
                                                },
                                            )}
                                        </span>
                                    </>
                                )}
                                {resolutionToast.phase === "failed" && (
                                    <>
                                        <AlertCircle className="w-4 h-4" />
                                        <span>
                                            {t(
                                                "pages.desk.resolutionFailed",
                                                "Update failed: {{reason}}",
                                                { reason: resolutionToast.reason },
                                            )}
                                        </span>
                                    </>
                                )}
                            </div>
                        )}

                        {isConnected && (
                            <div
                                ref={controlBarRef}
                                className="controlBar"
                                onMouseEnter={() => setIsControlBarHovered(true)}
                                onMouseLeave={() => setIsControlBarHovered(false)}
                            >
                                <div
                                    className="controlBarDragHandle"
                                    onMouseDown={handleDragStart}
                                    onTouchStart={() => setIsControlBarHovered(!isControlBarHovered)}
                                >
                                    <Menu className="w-5 h-5" />
                                </div>
                                <div
                                    className={`controlBarContent ${isControlBarExpanded ? 'expanded' : 'collapsed'}`}
                                    onFocus={() => setIsControlBarHovered(true)}
                                    onBlur={(e) => {
                                        if (!controlBarRef.current?.contains(e.relatedTarget as Node)) {
                                            setIsControlBarHovered(false);
                                        }
                                    }}
                                    inert={!isControlBarExpanded ? true : undefined}
                                >
                                    <div className="controlButtons">
                                    <Tooltip>
                                        <TooltipTrigger asChild>
                                            <Button
                                                variant="ghost"
                                                className="controlButton"
                                                onClick={handleRequestControl}
                                                disabled={isWaitingApproval}
                                            >
                                                {isWaitingApproval ? <Loader2 className="animate-spin" /> : hasControl ? <XSquare /> : <MousePointer2 />}
                                            </Button>
                                        </TooltipTrigger>
                                        <TooltipContent>
                                            <p>{isWaitingApproval ? t('pages.desk.waitingPermission', 'Waiting for host authorization dialog.') : hasControl ? t('pages.desk.exitControl', 'Exit Control') : t('pages.desk.requestControl', 'Request Control')}</p>
                                        </TooltipContent>
                                    </Tooltip>

                                    <Tooltip>
                                        <TooltipTrigger asChild>
                                            <Button
                                                variant="ghost"
                                                className="controlButton"
                                                onClick={handleFullscreen}
                                            >
                                                {isFullscreen ? <Minimize /> : <Maximize />}
                                            </Button>
                                        </TooltipTrigger>
                                        <TooltipContent>
                                            <p>{isFullscreen ? t('pages.desk.exitFullscreen', 'Exit Fullscreen') : t('pages.desk.fullscreen', 'Fullscreen')}</p>
                                        </TooltipContent>
                                    </Tooltip>

                                    <Tooltip>
                                        <TooltipTrigger asChild>
                                            <Button
                                                variant="ghost"
                                                className="controlButton"
                                                onClick={() => setIsConfigOpen(true)}
                                            >
                                                <Settings />
                                            </Button>
                                        </TooltipTrigger>
                                        <TooltipContent>
                                            <p>{t('pages.desk.settings', 'Settings')}</p>
                                        </TooltipContent>
                                    </Tooltip>

                                    <Tooltip>
                                        <TooltipTrigger asChild>
                                            <Button
                                                variant="ghost"
                                                className={`controlButton ${showStats ? "bg-white/20" : ""}`}
                                                onClick={() => setShowStats(!showStats)}
                                            >
                                                <Activity />
                                            </Button>
                                        </TooltipTrigger>
                                        <TooltipContent>
                                            <p>{showStats ? t('pages.desk.hideStats', 'Hide Network Stats') : t('pages.desk.showStats', 'Show Network Stats')}</p>
                                        </TooltipContent>
                                    </Tooltip>

                                    {/* Rainbow gradient definition for the AI Diagnose icon,
                                        referenced by `stroke="url(#ai-rainbow-gradient)"`. */}
                                    <svg width="0" height="0" className="absolute h-0 w-0" aria-hidden="true">
                                        <defs>
                                            <linearGradient id="ai-rainbow-gradient" x1="0%" y1="0%" x2="100%" y2="100%">
                                                <stop offset="0%" stopColor="#3b82f6">
                                                    <animate attributeName="stop-color" values="#3b82f6; #8b5cf6; #d946ef; #f43f5e; #3b82f6" dur={isDiagnoseHovered ? "0.8s" : "4s"} repeatCount="indefinite" />
                                                </stop>
                                                <stop offset="33%" stopColor="#8b5cf6">
                                                    <animate attributeName="stop-color" values="#8b5cf6; #d946ef; #f43f5e; #3b82f6; #8b5cf6" dur={isDiagnoseHovered ? "0.8s" : "4s"} repeatCount="indefinite" />
                                                </stop>
                                                <stop offset="66%" stopColor="#d946ef">
                                                    <animate attributeName="stop-color" values="#d946ef; #f43f5e; #3b82f6; #8b5cf6; #d946ef" dur={isDiagnoseHovered ? "0.8s" : "4s"} repeatCount="indefinite" />
                                                </stop>
                                                <stop offset="100%" stopColor="#f43f5e">
                                                    <animate attributeName="stop-color" values="#f43f5e; #3b82f6; #8b5cf6; #d946ef; #f43f5e" dur={isDiagnoseHovered ? "0.8s" : "4s"} repeatCount="indefinite" />
                                                </stop>
                                            </linearGradient>
                                        </defs>
                                    </svg>

                                    <Tooltip>
                                        <TooltipTrigger asChild>
                                            <Button
                                                variant="ghost"
                                                className={`controlButton ${showDiagnose ? "bg-white/20" : ""}`}
                                                onClick={() => setShowDiagnose(!showDiagnose)}
                                                onMouseEnter={() => setIsDiagnoseHovered(true)}
                                                onMouseLeave={() => setIsDiagnoseHovered(false)}
                                            >
                                                <Sparkles style={{ stroke: "url(#ai-rainbow-gradient)" }} />
                                            </Button>
                                        </TooltipTrigger>
                                        <TooltipContent>
                                            <p>{showDiagnose ? t('pages.desk.diagnose.hidePanel', 'Hide AI Diagnose') : t('pages.desk.diagnose.showPanel', 'AI Diagnose')}</p>
                                        </TooltipContent>
                                    </Tooltip>

                                    {hasControl && isPrivateScreenSupported && (
                                        <Tooltip>
                                            <TooltipTrigger asChild>
                                                <Button
                                                    variant="ghost"
                                                    className={`controlButton ${isPrivateScreen ? "bg-white/20 text-green-400" : ""}`}
                                                    onClick={handleTogglePrivateScreen}
                                                >
                                                    {isPrivateScreen ? <ShieldCheck /> : <ShieldOff />}
                                                </Button>
                                            </TooltipTrigger>
                                            <TooltipContent>
                                                <p>{isPrivateScreen ? t('pages.desk.disablePrivateScreen', 'Disable Privacy Screen') : t('pages.desk.enablePrivateScreen', 'Enable Privacy Screen')}</p>
                                            </TooltipContent>
                                        </Tooltip>
                                    )}

                                    {hasControl && (
                                        <Tooltip>
                                            <TooltipTrigger asChild>
                                                <Button
                                                    variant="ghost"
                                                    className={`controlButton ${clipboardEnabled ? "bg-white/20 text-blue-400" : "text-white/50"}`}
                                                    onClick={(e) => {
                                                        e.preventDefault();
                                                        console.log("Clipboard Toggle Button Hit");
                                                        if (typeof toggleClipboard === 'function') {
                                                            toggleClipboard();
                                                        } else {
                                                            console.error("toggleClipboard is not a function!");
                                                        }
                                                    }}
                                                >
                                                    {clipboardEnabled ? <Clipboard /> : <ClipboardX />}
                                                </Button>
                                            </TooltipTrigger>
                                            <TooltipContent>
                                                <p>{clipboardEnabled ? t('pages.desk.disableClipboardSync', 'Disable Clipboard Sync') : t('pages.desk.enableClipboardSync', 'Enable Clipboard Sync')} {!window.isSecureContext ? t('pages.desk.clipboardHttpsRequired', '(HTTPS Required)') : ''}</p>
                                            </TooltipContent>
                                        </Tooltip>
                                    )}

                                    {hasControl && (
                                        <DropdownMenu>
                                            <DropdownMenuTrigger asChild>
                                                <Button variant="ghost" className="controlButton">
                                                    <Keyboard />
                                                </Button>
                                            </DropdownMenuTrigger>
                                            <DropdownMenuContent align="end" className="w-56 bg-background/90 backdrop-blur-md border-white/10">
                                                <DropdownMenuItem onClick={() => {
                                                    sendKeyboardEvents([
                                                        { event: "keydown", keyCode: 17 }, // Ctrl
                                                        { event: "keydown", keyCode: 18 }, // Alt
                                                        { event: "keydown", keyCode: 46 }, // Del
                                                        { event: "keyup", keyCode: 46 },
                                                        { event: "keyup", keyCode: 18 },
                                                        { event: "keyup", keyCode: 17 },
                                                    ]);
                                                }}>
                                                    Ctrl + Alt + Del
                                                </DropdownMenuItem>
                                                <DropdownMenuItem onClick={() => {
                                                    sendKeyboardEvents([
                                                        { event: "keydown", keyCode: 17 }, // Ctrl
                                                        { event: "keydown", keyCode: 16 }, // Shift
                                                        { event: "keydown", keyCode: 27 }, // Esc
                                                        { event: "keyup", keyCode: 27 },
                                                        { event: "keyup", keyCode: 16 },
                                                        { event: "keyup", keyCode: 17 },
                                                    ]);
                                                }}>
                                                    Ctrl + Shift + Esc (任务管理器)
                                                </DropdownMenuItem>
                                                <DropdownMenuItem onClick={() => {
                                                    sendKeyboardEvents([
                                                        { event: "keydown", keyCode: 18 }, // Alt
                                                        { event: "keydown", keyCode: 115 }, // F4
                                                        { event: "keyup", keyCode: 115 },
                                                        { event: "keyup", keyCode: 18 },
                                                    ]);
                                                }}>
                                                    Alt + F4
                                                </DropdownMenuItem>
                                                <DropdownMenuItem onClick={() => {
                                                    sendKeyboardEvents([
                                                        { event: "keydown", keyCode: 18 }, // Alt
                                                        { event: "keydown", keyCode: 9 }, // Tab
                                                        { event: "keyup", keyCode: 9 },
                                                        { event: "keyup", keyCode: 18 },
                                                    ]);
                                                }}>
                                                    Alt + Tab (切换窗口)
                                                </DropdownMenuItem>
                                                <DropdownMenuItem onClick={() => {
                                                    sendKeyboardEvents([
                                                        { event: "keydown", keyCode: 91 }, // Win
                                                        { event: "keyup", keyCode: 91 },
                                                    ]);
                                                }}>
                                                    Windows Key
                                                </DropdownMenuItem>
                                                <DropdownMenuItem onClick={() => {
                                                    sendKeyboardEvents([
                                                        { event: "keydown", keyCode: 91 }, // Win
                                                        { event: "keydown", keyCode: 68 }, // D
                                                        { event: "keyup", keyCode: 68 },
                                                        { event: "keyup", keyCode: 91 },
                                                    ]);
                                                }}>
                                                    Win + D (显示桌面)
                                                </DropdownMenuItem>
                                                <DropdownMenuItem onClick={() => {
                                                    sendKeyboardEvents([
                                                        { event: "keydown", keyCode: 91 }, // Win
                                                        { event: "keydown", keyCode: 69 }, // E
                                                        { event: "keyup", keyCode: 69 },
                                                        { event: "keyup", keyCode: 91 },
                                                    ]);
                                                }}>
                                                    Win + E (打开资源管理器)
                                                </DropdownMenuItem>
                                                <DropdownMenuItem onClick={() => {
                                                    sendKeyboardEvents([
                                                        { event: "keydown", keyCode: 91 }, // Win
                                                        { event: "keydown", keyCode: 82 }, // R
                                                        { event: "keyup", keyCode: 82 },
                                                        { event: "keyup", keyCode: 91 },
                                                    ]);
                                                }}>
                                                    Win + R (运行)
                                                </DropdownMenuItem>
                                                <DropdownMenuItem onClick={() => {
                                                    sendKeyboardEvents([
                                                        { event: "keydown", keyCode: 91 }, // Win
                                                        { event: "keydown", keyCode: 76 }, // L
                                                        { event: "keyup", keyCode: 76 },
                                                        { event: "keyup", keyCode: 91 },
                                                    ]);
                                                }}>
                                                    Win + L (锁定计算机)
                                                </DropdownMenuItem>
                                            </DropdownMenuContent>
                                        </DropdownMenu>
                                    )}

                                    {/* Whiteboard button */}
                                    <Tooltip>
                                        <TooltipTrigger asChild>
                                            <Button
                                                variant="ghost"
                                                className={`controlButton ${whiteboard.isActive ? 'text-yellow-400' : ''}`}
                                                onClick={whiteboard.toggleWhiteboard}
                                                disabled={!whiteboard.canActivate}
                                            >
                                                <PenTool />
                                            </Button>
                                        </TooltipTrigger>
                                        <TooltipContent>
                                            <p>{whiteboard.canActivate
                                                ? (whiteboard.isActive ? t('pages.desk.closeWhiteboard', 'Close Whiteboard') : t('pages.desk.openWhiteboard', 'Open Whiteboard'))
                                                : t('pages.desk.whiteboardUnavailable', 'Whiteboard requires Tauri on remote')}
                                            </p>
                                        </TooltipContent>
                                    </Tooltip>

                                    {/* Microphone button */}
                                    <Tooltip>
                                        <TooltipTrigger asChild>
                                            <Button
                                                variant="ghost"
                                                className={`controlButton ${microphone.isMicActive ? 'text-green-400' : ''} ${microphone.micError ? 'text-red-400' : ''}`}
                                                onClick={microphone.toggleMicrophone}
                                            >
                                                {microphone.isMicActive ? <Mic /> : <MicOff />}
                                            </Button>
                                        </TooltipTrigger>
                                        <TooltipContent>
                                            <p>{microphone.micError
                                                ? microphone.micError
                                                : (microphone.isMicActive ? t('pages.desk.stopMic', 'Stop Microphone') : t('pages.desk.startMic', 'Start Microphone'))
                                            }</p>
                                        </TooltipContent>
                                    </Tooltip>

                                    <Popover onOpenChange={setIsControlBarMenuOpen}>
                                        <PopoverTrigger asChild>
                                            <Button
                                                variant="ghost"
                                                className="controlButton"
                                            >
                                                {isMuted || audioVolume === 0 ? <VolumeX /> : <Volume2 />}
                                            </Button>
                                        </PopoverTrigger>
                                        <PopoverContent side="top" className="w-32 px-3 py-4 flex flex-col items-center gap-2 bg-background/90 backdrop-blur-md border-white/10" align="center" sideOffset={16} onOpenAutoFocus={(e) => e.preventDefault()}>
                                            <Slider
                                                min={0}
                                                max={100}
                                                step={1}
                                                value={[audioVolume]}
                                                onValueChange={(vals) => handleVolumeChange(vals[0])}
                                                className="w-full"
                                            />
                                        </PopoverContent>
                                    </Popover>

                                    <div className="w-px h-6 bg-white/20 mx-1" />

                                    <Tooltip>
                                        <TooltipTrigger asChild>
                                            <Button
                                                variant="ghost"
                                                className="controlButton text-red-500 hover:text-red-400 hover:bg-red-500/20"
                                                onClick={handleDisconnect}
                                            >
                                                <Power />
                                            </Button>
                                        </TooltipTrigger>
                                        <TooltipContent>
                                            <p>{t('pages.desk.disconnect', 'Disconnect')}</p>
                                        </TooltipContent>
                                    </Tooltip>
                                    </div>
                                </div>
                            </div>
                        )}
                    </div>
                </TooltipProvider>
            </div >
        </div >
    )
}
