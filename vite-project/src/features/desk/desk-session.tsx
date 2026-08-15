import { useEffect, useRef, useState, useCallback, useMemo } from "react"
import { useParams, useNavigate } from "react-router-dom"
import { useTranslation } from "react-i18next"
import { v4 } from "uuid"
import { AlertTriangle, Loader2 } from "lucide-react"
import { TooltipProvider } from "@/components/ui/tooltip"

import { Input } from "@/components/ui/input"
import { Button } from "@/components/ui/button"
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
import { deskErrorMessage, type ErrorCodeKeyMap } from "@/lib/desk-error-i18n"
import type {
    MediaPipelineStateData,
    RemoteSessionSettings,
    RemoteSessionSettingsApplied,
    SystemAudioCaptureStateData,
} from "@/services/types"
import { deskErrorCodeEnum } from "@/services/types"
import { useRestrictedSession } from "@/features/desk/restricted-session"
import {
    armControlReconnect,
    armSettingsApplyTimeout,
    buildDesktopRequestRemotePayload,
    claimControlReconnect,
    isRemoteSettingsFailure,
    resolveAdaptiveBitrateForHost,
    shouldOpenConfigDialog,
    shouldShowMediaPipelineOverlay,
    systemAudioStateTranslationKey,
    type ControlReconnectIntent,
    videoSettingsMayInterrupt,
    waitForVideoPresentation,
} from "./desk-session-model"
import {
    ClipboardFallbackToast,
    ConnectionQualityBadge,
    DeskSessionStats,
    ResolutionStatusToast,
} from "./desk-session-panels"
import { DeskControlBar } from "./desk-control-bar"
import { getMacKeyboardMappingController } from "./keyboard-mapping"
import { useDraggableControlBar } from "./use-draggable-control-bar"
import {
    SIGNALING_TYPE_CODE_REQUEST_REMOTE_ACCESS,
    SIGNALING_TYPE_CODE_REQUIRE_CONTROL,
    SIGNALING_TYPE_CODE_RELEASE_CONTROL,
    SIGNALING_TYPE_CODE_CONTROL_RELEASED,
    SIGNALING_TYPE_CODE_CLOSE_REMOTE_SESSION,
    SIGNALING_TYPE_CODE_CONTROL_ACCEPTED,
    SIGNALING_TYPE_CODE_CONTROL_DENIED,
    SIGNALING_TYPE_CODE_APPLY_REMOTE_SESSION_SETTINGS,
    SIGNALING_TYPE_CODE_REMOTE_SESSION_SETTINGS_APPLIED,
    SIGNALING_TYPE_CODE_UPDATE_ADAPTIVE_VIDEO_QUALITY,
    SIGNALING_TYPE_CODE_SYSTEM_AUDIO_CAPTURE_STATE_CHANGED,
    SIGNALING_TYPE_CODE_SET_PRIVATE_SCREEN_VISIBILITY,
    SIGNALING_TYPE_CODE_PRIVATE_SCREEN_VISIBILITY_SET,
    SIGNALING_TYPE_CODE_PRIVATE_SCREEN_STATE_CHANGED,
    SIGNALING_TYPE_CODE_AUDIO_PLAYBACK_FAILED,
    SIGNALING_TYPE_CODE_MEDIA_PIPELINE_STATE_CHANGED,
    SIGNALING_TYPE_CODE_RETRY_MEDIA_PIPELINE,
    SIGNALING_TYPE_CODE_MEDIA_PIPELINE_RETRY_COMPLETED,
    SIGNALING_TYPE_CODE_CHANGE_DISPLAY_SETTINGS,
    SIGNALING_TYPE_CODE_DISPLAY_SETTINGS_CHANGED,
    SIGNALING_TYPE_CODE_ERROR,
    SIGNALING_TYPE_CODE_REMOTE_ACCESS_INITIALIZED,
    SIGNALING_TYPE_CODE_OFFER,
} from "./constants"
import { AdmissionRetrySchedule } from "./admission-retry"
import { usePrivateScreenPending } from "./use-private-screen-pending"
import { useDeviceConnectionResolution } from "@/hooks/use-device-id"
import {
    DEFAULT_DESK_USER_PREFERENCES,
    DeskPreferenceStore,
    type DeskDevicePreferencesV1,
    type WaylandControlMode,
} from "./desk-preferences"
import type { DeskConfigSubmission } from "./desk-config-model"

function executableSessionSettings(settings: DeskConfigSubmission): RemoteSessionSettings {
    const {
        adaptive_web_page_resolution: _adaptiveResolution,
        wayland_control_mode: _waylandControlMode,
        ...remote
    } = settings;
    return remote;
}

const MEDIA_PIPELINE_ERROR_KEYS: ErrorCodeKeyMap = {
    [deskErrorCodeEnum.VIDEO_ENCODER_DIMENSIONS_UNSUPPORTED]: "pages.desk.mediaPipeline.blockedDescription",
    [deskErrorCodeEnum.VIDEO_ENCODER_PREPARE_FAILED]: "pages.desk.mediaPipeline.prepareFailedDescription",
    [deskErrorCodeEnum.VIDEO_PIPELINE_RENEGOTIATION_REQUIRED]: "pages.desk.mediaPipeline.renegotiateDescription",
    [deskErrorCodeEnum.VIDEO_PIPELINE_RESTART_FAILED]: "pages.desk.mediaPipeline.retryFailedDescription",
    [deskErrorCodeEnum.VIDEO_PIPELINE_RUNTIME_FAILED]: "pages.desk.mediaPipeline.runtimeFailedDescription",
}

const SETTINGS_ERROR_KEYS: ErrorCodeKeyMap = {
    [deskErrorCodeEnum.PERMISSION_ERROR]: "pages.desk.systemAudioPermissionDenied",
    [deskErrorCodeEnum.FEATURE_UNAVAILABLE]: "pages.desk.settingFeatureUnavailable",
    [deskErrorCodeEnum.ACTION_NEED_RETRY]: "pages.desk.settingNeedsRetry",
    [deskErrorCodeEnum.ADAPTIVE_RESOLUTION_REQUIRES_SINGLE_CLIENT]: "pages.desk.adaptiveResolutionMultipleClients",
    [deskErrorCodeEnum.REMOTE_DESKTOP_CAPABILITIES_NOT_READY]: "pages.desk.capabilitiesNotReady",
    [deskErrorCodeEnum.MEDIA_WORKER_RESTART_REQUIRED]: "pages.desk.mediaWorkerRestartRequired",
}

const SETTINGS_EFFECT_KEYS: Record<string, string> = {
    unchanged: "pages.desk.settingsEffect.unchanged",
    applied_live: "pages.desk.settingsEffect.appliedLive",
    restarted: "pages.desk.settingsEffect.restarted",
    started: "pages.desk.settingsEffect.started",
    stopped: "pages.desk.settingsEffect.stopped",
    needs_reconnect: "pages.desk.settingsEffect.needsReconnect",
}

/** Container props. `orgId` is injected only by the manager console's org view
 *  (via a static wrapper); the open-source standalone app renders `<DeskSession/>`
 *  with no props, keeping the AI model selection personal-scoped. */
type DeskSessionProps = {
    orgId?: number
    /** Manager injects `u:<user_id>`; standalone omits it for its fixed owner. */
    preferenceOwnerKey?: string | null
    /** Manager identity query is still pending; admission must wait. */
    preferenceOwnerLoading?: boolean
}

export default function DeskSession({
    orgId,
    preferenceOwnerKey,
    preferenceOwnerLoading = false,
}: DeskSessionProps = {}) {
    const { id: deskId } = useParams<{ id: string }>()
    const navigate = useNavigate()
    const { t } = useTranslation()
    const { toast } = useToast()

    // Control state
    const [hasControl, setHasControl] = useState(false);
    const [isWaitingApproval, setIsWaitingApproval] = useState(false);
    const controlRequestRef = useRef<{
        requestId: string;
        kind: "require" | "release";
        wantsClipboard?: boolean;
    } | null>(null);
    const controlReconnectRef = useRef<ControlReconnectIntent | null>(null);
    const clipboardEnabledRef = useRef(false);
    const clipboardReconnectIntentRef = useRef<boolean | null>(null);
    const deactivateWhiteboardRef = useRef<() => void>(() => {});
    const hasRequestedRef = useRef(false);
    const admissionRetryRef = useRef({
        generation: 0,
        requestIds: new Set<string>(),
        schedule: new AdmissionRetrySchedule(),
        timer: null as number | null,
    });

    const { isConnected, subscribe, sendMessage, sendTracked, cancelQueued, cancelQueuedScope } = useDeskSignaling()

    // Restriction state derived from the redeemed grant (if any) for this target.
    const restricted = useRestrictedSession(deskId);
    const grantSessionId = restricted.grantSessionId;
    // Grant classification happens before resolving any persistent device key.
    // Restricted sessions are always memory-only even if the target itself has
    // a stable manager device id.
    const deviceConnectionResolution = useDeviceConnectionResolution(deskId);
    const controllerUserKey = preferenceOwnerKey === undefined
        ? "standalone-owner"
        : preferenceOwnerKey;
    const deviceKey = deviceConnectionResolution.status === "persistent"
        ? deviceConnectionResolution.deviceKey
        : null;
    const preferenceScope = useMemo(() => ({
        controllerUserKey,
        deviceKey,
        restricted: restricted.isRestricted,
    }), [controllerUserKey, deviceKey, restricted.isRestricted]);
    const preferenceStoreRef = useRef<DeskPreferenceStore | null>(null);
    if (preferenceStoreRef.current === null) {
        let storage: Storage | null = null;
        try {
            storage = typeof window === "undefined" ? null : window.localStorage;
        } catch {
            storage = null;
        }
        preferenceStoreRef.current = new DeskPreferenceStore(storage);
    }
    const admittedWaylandModeRef = useRef<WaylandControlMode>("auto");

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
        if (
            !deskId
            || preferenceOwnerLoading
            || deviceConnectionResolution.status === "loading"
        ) return;
        const state = admissionRetryRef.current;
        if (newLogicalAttempt) {
            clearAdmissionRetry();
            // Admission consumes the already-hydrated stable scope. The old
            // connection-id-keyed Wayland localStorage path is intentionally
            // gone; a missing stable identity remains page-memory-only.
            admittedWaylandModeRef.current = preferenceStoreRef.current!
                .loadDevice(preferenceScope)
                .waylandControlMode;
        }
        const requestData = buildDesktopRequestRemotePayload(
            deskId,
            grantSessionId,
            admittedWaylandModeRef.current,
        )
        const requestId = sendMessage(
            SIGNALING_TYPE_CODE_REQUEST_REMOTE_ACCESS,
            requestData,
            deskId,
        );
        state.requestIds.add(requestId);
        hasRequestedRef.current = true;
    }, [
        clearAdmissionRetry,
        deskId,
        deviceConnectionResolution.status,
        grantSessionId,
        preferenceOwnerLoading,
        preferenceScope,
        sendMessage,
    ]);

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
    const [devicePreferences, setDevicePreferences] =
        useState<DeskDevicePreferencesV1 | null>(null);
    // Deliberately reload on every dialog open. This gives another tab's last
    // completed save a chance to become visible without live storage events.
    useEffect(() => {
        if (
            !isConfigOpen
            || preferenceOwnerLoading
            || deviceConnectionResolution.status === "loading"
        ) return;
        setDevicePreferences(
            preferenceStoreRef.current!.loadDeviceIfPresent(preferenceScope),
        );
    }, [
        deviceConnectionResolution.status,
        isConfigOpen,
        preferenceOwnerLoading,
        preferenceScope,
    ]);
    // True once the user has kicked off a WebRTC connect from the dialog. Gates
    // the auto-reopen so a transient ICE `disconnected` during/after negotiation
    // does not pop the dialog back up over a connection that is still healing.
    const hasAttemptedConnectRef = useRef(false);
    const [isVideoReady, setIsVideoReady] = useState(false);
    const [isVideoTransitioning, setIsVideoTransitioning] = useState(false);
    const videoTransitionGenerationRef = useRef(0);
    const pendingVideoFrameCancelRef = useRef<(() => void) | null>(null);
    const clearPendingVideoFrameWait = useCallback(() => {
        pendingVideoFrameCancelRef.current?.();
        pendingVideoFrameCancelRef.current = null;
    }, []);
    const beginVideoTransition = useCallback(() => {
        clearPendingVideoFrameWait();
        videoTransitionGenerationRef.current += 1;
        setIsVideoTransitioning(true);
    }, [clearPendingVideoFrameWait]);
    const finishVideoTransition = useCallback((generation?: number) => {
        if (generation !== undefined && generation !== videoTransitionGenerationRef.current) return;
        clearPendingVideoFrameWait();
        setIsVideoTransitioning(false);
    }, [clearPendingVideoFrameWait]);
    const finishVideoTransitionOnNextFrame = useCallback(() => {
        clearPendingVideoFrameWait();
        const video = videoRef.current;
        if (!video) return;
        const generation = videoTransitionGenerationRef.current;
        pendingVideoFrameCancelRef.current = waitForVideoPresentation(video, () => {
            pendingVideoFrameCancelRef.current = null;
            finishVideoTransition(generation);
        });
    }, [clearPendingVideoFrameWait, finishVideoTransition]);
    useEffect(() => () => {
        videoTransitionGenerationRef.current += 1;
        clearPendingVideoFrameWait();
    }, [clearPendingVideoFrameWait]);
    const [mediaPipelineState, setMediaPipelineState] = useState<MediaPipelineStateData | null>(null);
    const [mediaRetryPending, setMediaRetryPending] = useState(false);
    const mediaRetryRequestIdRef = useRef<string | null>(null);
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

    const [adaptiveQualityEnabled, setAdaptiveQualityEnabled] =
        useState(DEFAULT_DESK_USER_PREFERENCES.adaptiveQualityEnabled);
    const [adaptiveBitrateEnabled, setAdaptiveBitrateEnabled] =
        useState(DEFAULT_DESK_USER_PREFERENCES.adaptiveBitrateEnabled);
    const persistedUserPreferenceKey = restricted.isRestricted
        ? null
        : controllerUserKey;
    const userPreferenceScope = persistedUserPreferenceKey ?? "memory-only";
    const [hydratedUserPreferenceScope, setHydratedUserPreferenceScope] =
        useState<string | null>(null);
    useEffect(() => {
        const preferences = preferenceStoreRef.current!.loadUser(
            persistedUserPreferenceKey,
        );
        setAdaptiveQualityEnabled(preferences.adaptiveQualityEnabled);
        setAdaptiveBitrateEnabled(preferences.adaptiveBitrateEnabled);
        setHydratedUserPreferenceScope(userPreferenceScope);
    }, [persistedUserPreferenceKey, userPreferenceScope]);
    useEffect(() => {
        if (hydratedUserPreferenceScope !== userPreferenceScope) return;
        preferenceStoreRef.current!.saveUser(persistedUserPreferenceKey, {
            version: 1,
            adaptiveQualityEnabled,
            adaptiveBitrateEnabled,
        });
    }, [
        adaptiveBitrateEnabled,
        adaptiveQualityEnabled,
        hydratedUserPreferenceScope,
        persistedUserPreferenceKey,
        userPreferenceScope,
    ]);

    // Privacy screen state
    const [isPrivateScreen, setIsPrivateScreen] = useState(false);
    const [isPrivateScreenSupported, setIsPrivateScreenSupported] = useState(true);
    const releaseInputsRef = useRef<() => void>(() => {});
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

    const settingsReconnectRef = useRef<{
        previousEpoch: string;
        settings: RemoteSessionSettings;
    } | null>(null);
    const { peerConnection, remoteStream, initData, connect, mouseChannel, keyboardChannel, mouseMoveChannel, clipboardChannel, whiteboardChannel, cursorSyncChannel, isRTCConnected, rtcFailed, closeRTC, rtcStats } = useDeskRTC({
        deskId: deskId || null,
        subscribe,
        sendTracked,
        cancelQueued,
        cancelQueuedScope,
    });

    const clearSettingsApply = useCallback(() => {
        const pending = settingsApplyRef.current;
        if (pending?.timer !== null && pending?.timer !== undefined) {
            window.clearTimeout(pending.timer);
        }
        settingsApplyRef.current = null;
    }, []);

    const rebuildRemoteSession = useCallback((settings: RemoteSessionSettings) => {
        if (!deskId || !initData) return;
        beginVideoTransition();
        const previousEpoch = initData.connection_epoch;
        releaseInputsRef.current();
        controlReconnectRef.current = armControlReconnect(hasControl, previousEpoch);
        clipboardReconnectIntentRef.current = hasControl ? clipboardEnabledRef.current : null;
        controlRequestRef.current = null;
        setHasControl(false);
        setIsWaitingApproval(false);
        if (isConnected) {
            // The socket is open, so this Close goes on the wire immediately;
            // RequestRemoteAccess below follows it in WebSocket FIFO order.
            sendMessage(
                SIGNALING_TYPE_CODE_CLOSE_REMOTE_SESSION,
                {
                    connection_epoch: previousEpoch,
                    finalize_logical_connection: false,
                },
                deskId,
            );
        }
        clearSettingsApply();
        closeRTC();
        setSystemAudioState(null);
        settingsReconnectRef.current = { previousEpoch, settings };
        hasRequestedRef.current = false;
        sendRemoteAdmission(true);
    }, [
        beginVideoTransition,
        clearSettingsApply,
        closeRTC,
        deskId,
        hasControl,
        initData,
        isConnected,
        sendMessage,
        sendRemoteAdmission,
    ]);

    useEffect(() => {
        const pending = settingsReconnectRef.current;
        if (!pending || !initData || initData.connection_epoch === pending.previousEpoch) return;
        settingsReconnectRef.current = null;
        hasAttemptedConnectRef.current = true;
        setMediaRetryPending(false);
        void connect(pending.settings);
    }, [connect, initData]);

    const applyRemoteSettings = useCallback((
        settings: DeskConfigSubmission,
        mayInterruptVideo = false,
    ) => {
        if (!deskId || !initData) return;
        if (settingsApplyRef.current) {
            toast({ title: t("pages.desk.settingsApplyInProgress") });
            return;
        }
        if (mayInterruptVideo) beginVideoTransition();
        const connectionEpoch = initData.connection_epoch;
        const requested = executableSessionSettings(settings);
        const requestId = v4();
        const replaceKey = `session-settings:${connectionEpoch}`;
        settingsApplyRef.current = {
            requestId,
            connectionEpoch,
            requested: settings,
            timer: null,
        };
        const result = sendTracked({
            type: SIGNALING_TYPE_CODE_APPLY_REMOTE_SESSION_SETTINGS,
            data: {
                connection_epoch: connectionEpoch,
                settings: requested,
            },
            toConnectionId: deskId,
            requestId,
            replaceKey,
            scope: `session:${connectionEpoch}`,
            onSent: (requestId) => {
                const pending = settingsApplyRef.current;
                if (!pending || pending.requestId !== requestId) return;
                armSettingsApplyTimeout(pending, requestId, () => {
                    if (settingsApplyRef.current?.requestId !== requestId) return;
                    settingsApplyRef.current = null;
                    finishVideoTransition();
                    toast({
                        variant: "destructive",
                        title: t("pages.desk.settingsApplyTimeout"),
                    });
                });
            },
        });
        if (result.requestId !== requestId) {
            clearSettingsApply();
            finishVideoTransition();
        } else if (result.disposition === "queued") {
            const pending = settingsApplyRef.current;
            if (pending) {
                armSettingsApplyTimeout(pending, requestId, () => {
                    if (settingsApplyRef.current?.requestId !== requestId) return;
                    cancelQueued(replaceKey);
                    settingsApplyRef.current = null;
                    finishVideoTransition();
                    toast({
                        variant: "destructive",
                        title: t("pages.desk.settingsApplyTimeout"),
                    });
                });
            }
        }
    }, [
        beginVideoTransition,
        cancelQueued,
        clearSettingsApply,
        deskId,
        finishVideoTransition,
        initData,
        sendTracked,
        t,
        toast,
    ]);

    useEffect(() => subscribe((message) => {
        if (message.signaling_type === SIGNALING_TYPE_CODE_SYSTEM_AUDIO_CAPTURE_STATE_CHANGED) {
            const state = message.signaling_data as SystemAudioCaptureStateData;
            if (state.connection_epoch === initData?.connection_epoch) {
                setSystemAudioState(state);
                const current = lastSettingsRef.current;
                if (current) {
                    const accepted = {
                        ...current,
                        audio: state.accepted_audio,
                    };
                    lastSettingsRef.current = accepted;
                    setActiveSettings(accepted);
                }
                if (state.error_code === deskErrorCodeEnum.MEDIA_WORKER_RESTART_REQUIRED) {
                    toast({
                        variant: "destructive",
                        title: t("pages.desk.mediaWorkerRestartRequired"),
                    });
                }
            }
            return;
        }
        if (message.signaling_type !== SIGNALING_TYPE_CODE_REMOTE_SESSION_SETTINGS_APPLIED
            && message.signaling_type !== SIGNALING_TYPE_CODE_ERROR) return;
        const pending = settingsApplyRef.current;
        if (!pending || message.request_id !== pending.requestId) return;
        if (isRemoteSettingsFailure(
            message.signaling_type === SIGNALING_TYPE_CODE_ERROR,
            message.response_state?.error_code,
        )) {
            clearSettingsApply();
            finishVideoTransition();
            toast({
                variant: "destructive",
                title: t("pages.desk.settingsApplyFailed"),
                description: deskErrorMessage(
                    t,
                    SETTINGS_ERROR_KEYS,
                    message.response_state?.error_code,
                    message.response_state?.message ?? undefined,
                    t("pages.desk.settingsApplyFailed"),
                ),
            });
            return;
        }
        if (message.signaling_type !== SIGNALING_TYPE_CODE_REMOTE_SESSION_SETTINGS_APPLIED) return;
        const applied = message.signaling_data as RemoteSessionSettingsApplied;
        if (applied.connection_epoch !== pending.connectionEpoch) return;
        clearSettingsApply();
        if (applied.effects.connection === "needs_reconnect") {
            toast({
                title: t("pages.desk.settingsReconnectRequired"),
                description: t("pages.desk.settingsReconnectStarting"),
            });
            hasAttemptedConnectRef.current = true;
            lastSettingsRef.current = pending.requested;
            setActiveSettings(pending.requested);
            adaptiveQualityOverrideRef.current = null;
            rebuildRemoteSession(executableSessionSettings(pending.requested));
            return;
        }
        if (applied.effects.video === "restarted") {
            finishVideoTransitionOnNextFrame();
        } else {
            finishVideoTransition();
        }
        const localSettings = pending.requested;
        const accepted: DeskConfigSubmission = {
            ...applied.baseline_settings,
            adaptive_web_page_resolution:
                localSettings?.adaptive_web_page_resolution ?? true,
            wayland_control_mode:
                localSettings?.wayland_control_mode ?? admittedWaylandModeRef.current,
        };
        lastSettingsRef.current = accepted;
        setActiveSettings(accepted);
        adaptiveQualityOverrideRef.current =
            applied.runtime_overrides.adaptive_video_quality ?? null;
        const effectDescription = t("pages.desk.settingsAppliedEffects", {
            video: t(SETTINGS_EFFECT_KEYS[applied.effects.video] ?? applied.effects.video),
            audio: t(SETTINGS_EFFECT_KEYS[applied.effects.audio] ?? applied.effects.audio),
            connection: t(SETTINGS_EFFECT_KEYS[applied.effects.connection]
                ?? applied.effects.connection),
        });
        const fieldErrors = applied.errors.map((error) => `${error.field}: ${deskErrorMessage(
            t,
            SETTINGS_ERROR_KEYS,
            error.code,
            undefined,
            t("pages.desk.settingsApplyFailed"),
        )}`).join("; ");
        toast({
            title: t("pages.desk.settingsApplied"),
            description: fieldErrors
                ? `${effectDescription}. ${fieldErrors}`
                : effectDescription,
        });
    }), [
        clearSettingsApply,
        finishVideoTransition,
        finishVideoTransitionOnNextFrame,
        initData?.connection_epoch,
        rebuildRemoteSession,
        subscribe,
        t,
        toast,
    ]);

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
    // `lastSettingsRef` is the host-accepted baseline for the active epoch.
    // A fresh Offer/rebuild seeds it with that Offer's complete settings;
    // live 301 changes replace it only after the correlated 302 response.
    const lastSettingsRef = useRef<DeskConfigSubmission | null>(null);
    // State mirror of the accepted settings used solely for
    // hooks/effects whose React-tracked deps must re-evaluate on a
    // settings change. Refs are intentionally invisible to React's
    // render cycle, so reading `lastSettingsRef.current` inside a hook's
    // `enabled` prop would silently miss "user changed display to the
    // IDD and ticked adaptive" until some other state happened to
    // re-render the component. Mirror only fields the gating cares
    // about — the adaptive-quality auto-adjust path keeps writing only
    // to the ref so its per-stats-tick updates do not force a re-render.
    const [activeSettings, setActiveSettings] = useState<DeskConfigSubmission | null>(null);
    const [systemAudioState, setSystemAudioState] = useState<SystemAudioCaptureStateData | null>(null);
    const settingsApplyRef = useRef<{
        requestId: string;
        connectionEpoch: string;
        requested: DeskConfigSubmission;
        timer: number | null;
    } | null>(null);
    const adaptiveQualityOverrideRef = useRef<number | null>(null);

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
            changeDisplaySettingsType: SIGNALING_TYPE_CODE_DISPLAY_SETTINGS_CHANGED,
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

    const { cursorStyle } = useCursorSync(
        cursorSyncChannel,
        videoRef,
        isRTCConnected && hasControl && (activeSettings?.show_mouse ?? true),
    );

    const {
        clipboardEnabled,
        transferProgress,
        transferStatus,
        errorMessage,
        toggleClipboard,
        enableClipboard,
        fallbackToast,
        execFallbackToastAction,
        closeFallbackToast
    } = useDeskClipboard({
        clipboardChannel,
        hasControl: isRTCConnected && hasControl,
        isActive: true
    });
    clipboardEnabledRef.current = clipboardEnabled;

    const whiteboard = useDeskWhiteboard({
        videoRef,
        whiteboardChannel,
        isConnected: isRTCConnected && hasControl,
        hasTauri: initData?.has_tauri ?? false,
    });
    deactivateWhiteboardRef.current = whiteboard.deactivateWhiteboard;

    const macKeyboardMappingController = getMacKeyboardMappingController(
        initData?.operation_system,
    );

    const { sendKeyboardEvents, releaseAllInputs } = useDeskInput({
        videoRef,
        mouseChannel,
        keyboardChannel,
        mouseMoveChannel,
        isConnected: isRTCConnected && hasControl, // Only enable inputs if we have control
        ignoreInputEvents: !!whiteboard.textInput,
        remapCtrlToCommand: macKeyboardMappingController !== undefined,
    });
    releaseInputsRef.current = releaseAllInputs;

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

            if (signaling_type === SIGNALING_TYPE_CODE_REMOTE_ACCESS_INITIALIZED
                && !message.response_state
                && message.request_id
                && admissionRetryRef.current.requestIds.has(message.request_id)
            ) {
                clearAdmissionRetry();
            } else if ((signaling_type === SIGNALING_TYPE_CODE_ERROR
                || (signaling_type === SIGNALING_TYPE_CODE_REMOTE_ACCESS_INITIALIZED
                    && !!message.response_state))
                && message.request_id
                && admissionRetryRef.current.requestIds.delete(message.request_id)
            ) {
                if (
                    message.response_state?.error_code === deskErrorCodeEnum.ACTION_NEED_RETRY
                    || message.response_state?.error_code
                        === deskErrorCodeEnum.REMOTE_DESKTOP_CAPABILITIES_NOT_READY
                ) {
                    const state = admissionRetryRef.current;
                    const delay = state.schedule.nextDelay();
                    if (delay === null) {
                        clearAdmissionRetry();
                        const abandoned = settingsReconnectRef.current;
                        if (abandoned && deskId && isConnected) {
                            sendMessage(
                                SIGNALING_TYPE_CODE_CLOSE_REMOTE_SESSION,
                                {
                                    connection_epoch: abandoned.previousEpoch,
                                    finalize_logical_connection: true,
                                },
                                deskId,
                            );
                            settingsReconnectRef.current = null;
                            controlReconnectRef.current = null;
                            clipboardReconnectIntentRef.current = null;
                            clearPrivateScreenPending();
                            setIsPrivateScreen(false);
                            finishVideoTransition();
                        }
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
                    if (message.response_state?.error_code
                        === deskErrorCodeEnum.WAYLAND_PORTAL_AUTHORIZATION_REQUIRED
                    ) {
                        toast({
                            title: t("pages.desk.waylandAuthorizationRequiredTitle"),
                            description: t("pages.desk.waylandAuthorizationRequiredDescription"),
                            variant: "destructive",
                        });
                    }
                }
            } else if (signaling_type === SIGNALING_TYPE_CODE_OFFER
                && message.response_state
            ) {
                setMediaRetryPending(false);
                setMediaPipelineState({
                    phase: message.response_state.error_code
                        === deskErrorCodeEnum.VIDEO_ENCODER_DIMENSIONS_UNSUPPORTED
                        ? "blocked"
                        : "failed",
                    reason_code: message.response_state.error_code as MediaPipelineStateData["reason_code"],
                    message: message.response_state.message,
                    compatible_encoders: [],
                });
            } else if ((signaling_type === SIGNALING_TYPE_CODE_CONTROL_ACCEPTED
                || signaling_type === SIGNALING_TYPE_CODE_CONTROL_DENIED)
                && message.request_id === controlRequestRef.current?.requestId
                && controlRequestRef.current?.kind === "require"
            ) {
                const request = controlRequestRef.current;
                controlRequestRef.current = null;
                if (signaling_type === SIGNALING_TYPE_CODE_CONTROL_DENIED) {
                    console.log("Remote control request DENIED by peer.");
                    deactivateWhiteboardRef.current();
                    setHasControl(false);
                    setIsWaitingApproval(false);
                    return;
                }
                console.log("Remote control request ACCEPTED by peer.");
                setHasControl(true);
                if (request?.wantsClipboard) enableClipboard();
                setIsWaitingApproval(false);
                videoRef.current?.focus();
            } else if (signaling_type === SIGNALING_TYPE_CODE_CONTROL_RELEASED
                && message.request_id === controlRequestRef.current?.requestId
                && controlRequestRef.current?.kind === "release"
            ) {
                controlRequestRef.current = null;
                console.log("Remote control RELEASED.");
                deactivateWhiteboardRef.current();
                setHasControl(false);
                setIsWaitingApproval(false);
            } else if (signaling_type === SIGNALING_TYPE_CODE_PRIVATE_SCREEN_VISIBILITY_SET) {
                const data = message.signaling_data;
                const requestId = message.request_id;
                if (!requestId) return;
                if (message.response_state?.error_code) {
                    const error = message.response_state.message ?? data?.error_msg;
                    console.error("Private screen error:", error);
                    failPrivateScreenPending(requestId, error);
                } else if (data) {
                    console.log("Private screen visibility set:", data);
                    setIsPrivateScreen(data.visible ?? false);
                    setIsPrivateScreenSupported(data.is_supported ?? true);
                    if (data.error_msg) {
                        const error = data.error_msg;
                        console.error("Private screen error:", error);
                        failPrivateScreenPending(requestId, error);
                    } else if (typeof data.visible === "boolean") {
                        confirmPrivateScreenPending(requestId, data.visible);
                    }
                }
            } else if (signaling_type === SIGNALING_TYPE_CODE_PRIVATE_SCREEN_STATE_CHANGED) {
                const data = message.signaling_data;
                if (data) {
                    setIsPrivateScreen(data.visible ?? false);
                    setIsPrivateScreenSupported(data.is_supported ?? true);
                }
            } else if (signaling_type === SIGNALING_TYPE_CODE_AUDIO_PLAYBACK_FAILED) {
                const data = message.signaling_data;
                if (data && data.error) {
                    console.error("Remote audio playback error:", data.error);
                    forceErrorRef.current(data.error);
                }
            } else if (signaling_type === SIGNALING_TYPE_CODE_MEDIA_PIPELINE_STATE_CHANGED) {
                const state = message.signaling_data as MediaPipelineStateData | null;
                if (state?.phase === "streaming") {
                    setMediaPipelineState(null);
                } else if (state) {
                    setMediaPipelineState(state);
                }
            } else if (signaling_type === SIGNALING_TYPE_CODE_MEDIA_PIPELINE_RETRY_COMPLETED
                && message.response_state
                && message.request_id === mediaRetryRequestIdRef.current
            ) {
                mediaRetryRequestIdRef.current = null;
                setMediaRetryPending(false);
                if (!message.response_state.error_code) {
                    setMediaPipelineState(null);
                    finishVideoTransitionOnNextFrame();
                    return;
                }
                finishVideoTransition();
                const needsRenegotiation = message.response_state.error_code
                    === deskErrorCodeEnum.VIDEO_PIPELINE_RENEGOTIATION_REQUIRED;
                if (needsRenegotiation) setIsConfigOpen(true);
                toast({
                    title: needsRenegotiation
                        ? t("pages.desk.mediaPipeline.renegotiateTitle")
                        : t("pages.desk.mediaPipeline.retryFailedTitle"),
                    description: deskErrorMessage(
                        t,
                        MEDIA_PIPELINE_ERROR_KEYS,
                        message.response_state.error_code,
                        message.response_state.message,
                        t("pages.desk.mediaPipeline.retryFailedDescription"),
                    ),
                    variant: "destructive",
                });
            } else if (signaling_type === SIGNALING_TYPE_CODE_DISPLAY_SETTINGS_CHANGED) {
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
    }, [
        clearAdmissionRetry,
        clearPrivateScreenPending,
        confirmPrivateScreenPending,
        deskId,
        enableClipboard,
        failPrivateScreenPending,
        finishVideoTransition,
        finishVideoTransitionOnNextFrame,
        isConnected,
        sendMessage,
        sendRemoteAdmission,
        subscribe,
        t,
        toast,
    ]);

    // Reset requested state if connection drops
    useEffect(() => {
        if (!isConnected) {
            clearAdmissionRetry();
            clearPrivateScreenPending();
            hasRequestedRef.current = false;
            setMediaPipelineState(null);
            setMediaRetryPending(false);
        }
    }, [clearAdmissionRetry, clearPrivateScreenPending, isConnected]);

    useEffect(() => clearAdmissionRetry, [clearAdmissionRetry]);

    // Wait for REMOTE_ACCESS_INITIALIZED data, then show the config dialog so the user can pick
    // capture settings. Reopen it only for the initial pick or after a terminal
    // ICE failure (retry) — never on a transient `disconnected`, which heals on
    // its own (see `shouldOpenConfigDialog`). The auto-reconnect path that fired
    // after `DesktopReady` is gone — the daemon-held PC survives worker swaps so
    // REMOTE_ACCESS_INITIALIZED only ever arrives once per session.
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
        // User-toggleable: when disabled, no narrow 303 override is sent
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
        const currentQuality = adaptiveQualityOverrideRef.current
            ?? lastSettingsRef.current.video_quality
            ?? 22;

        let newQuality: number | null = null;
        if ((avgPacketLoss > 3 || avgRtt > 200) && elapsed >= 3000) {
            newQuality = Math.min(63, currentQuality + 5);
        } else if (avgPacketLoss < 0.5 && avgRtt < 100 && elapsed >= 10000) {
            newQuality = Math.max(0, currentQuality - 2);
        }

        if (newQuality !== null && newQuality !== currentQuality) {
            sendTracked({
                type: SIGNALING_TYPE_CODE_UPDATE_ADAPTIVE_VIDEO_QUALITY,
                data: {
                    connection_epoch: initData?.connection_epoch,
                    video_quality: newQuality,
                },
                toConnectionId: deskId,
                replaceKey: `adaptive-quality:${initData?.connection_epoch ?? ""}`,
                scope: `session:${initData?.connection_epoch ?? ""}`,
            });
            adaptiveQualityOverrideRef.current = newQuality;
            lastQualityAdjustRef.current = now;
            statsWindowRef.current = [];
            qualityAdjustmentCountRef.current += 1;
        }
    }, [rtcStats, adaptiveQualityEnabled, sendTracked, deskId, initData?.connection_epoch]);

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
            if (!initData?.connection_epoch) return "";
            const wirePayload = {
                ...payload,
                connection_epoch: initData.connection_epoch,
            };
            const reqId = sendMessage(
                SIGNALING_TYPE_CODE_CHANGE_DISPLAY_SETTINGS,
                wirePayload,
                deskId ?? undefined,
            );
            console.info("[adaptive-resolution dispatch] 205 sent", {
                reqId,
                payload: wirePayload,
                deskId,
            });
            registerResolutionSent(reqId, payload.width, payload.height);
            return reqId;
        },
        [sendMessage, deskId, initData?.connection_epoch, registerResolutionSent],
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
    // A new Offer seeds `lastSettingsRef.current` before `connect`; live
    // changes update it only after the host confirms the accepted baseline.
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

    const handleConfigSubmit = (
        settings: DeskConfigSubmission,
        preferences: DeskDevicePreferencesV1,
    ) => {
        if (!initData) return;
        if (settingsApplyRef.current) {
            toast({ title: t("pages.desk.settingsApplyInProgress") });
            return;
        }
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
        const requestedMode = settings.wayland_control_mode ?? "auto";
        if (deskId && requestedMode !== admittedWaylandModeRef.current) {
            toast({
                title: t(isRTCConnected
                    ? "pages.desk.waylandModeReconnectTitle"
                    : "pages.desk.waylandModeNextConnectionTitle"),
                description: t(isRTCConnected
                    ? "pages.desk.waylandModeReconnectDescription"
                    : "pages.desk.waylandModeNextConnectionDescription"),
            });
        }
        const settingsWithPrefs: DeskConfigSubmission = {
            ...settings,
            adaptive_bitrate: resolveAdaptiveBitrateForHost(
                initData.session_settings_capabilities.adaptive_bitrate,
                initData.suggested_session_settings.adaptive_bitrate,
                adaptiveBitrateEnabled,
            ),
        };
        const previousSettings = lastSettingsRef.current;
        preferenceStoreRef.current!.saveDevice(preferenceScope, preferences);
        setDevicePreferences(preferences);
        if (isRTCConnected && requestedMode !== admittedWaylandModeRef.current) {
            lastSettingsRef.current = settingsWithPrefs;
            adaptiveQualityOverrideRef.current = null;
            setActiveSettings(settingsWithPrefs);
            hasAttemptedConnectRef.current = true;
            rebuildRemoteSession(executableSessionSettings(settingsWithPrefs));
        } else if (mediaPipelineState) {
            // A terminal video pipeline has already released its encoder and
            // capture subscription. Re-admit the logical session so all old
            // callbacks/queued controls are cancelled and the host allocates a
            // fresh connection epoch before applying the requested baseline.
            lastSettingsRef.current = settingsWithPrefs;
            adaptiveQualityOverrideRef.current = null;
            setActiveSettings(settingsWithPrefs);
            hasAttemptedConnectRef.current = true;
            setMediaRetryPending(true);
            beginVideoTransition();
            void (isRTCConnected
                ? rebuildRemoteSession(executableSessionSettings(settingsWithPrefs))
                : connect(executableSessionSettings(settingsWithPrefs)));
        } else if (isRTCConnected && deskId) {
            applyRemoteSettings(
                settingsWithPrefs,
                videoSettingsMayInterrupt(previousSettings, settingsWithPrefs),
            );
        } else {
            // Mark that a connect is in flight so a transient ICE drop during
            // negotiation does not auto-reopen this dialog.
            lastSettingsRef.current = settingsWithPrefs;
            adaptiveQualityOverrideRef.current = null;
            setActiveSettings(settingsWithPrefs);
            hasAttemptedConnectRef.current = true;
            connect(executableSessionSettings(settingsWithPrefs));
        }
        setIsConfigOpen(false);
    };

    // Live-apply the adaptive-bitrate toggle while connected: the
    // checkbox sits outside the dialog form, so a flip must reach the
    // daemon without waiting for the next full settings submit. The
    // daemon scopes the change to this connection.
    useEffect(() => {
        if (!isRTCConnected || !deskId || !lastSettingsRef.current) return;
        if (settingsApplyRef.current) return;
        const resolvedAdaptiveBitrate = initData
            ? resolveAdaptiveBitrateForHost(
                initData.session_settings_capabilities.adaptive_bitrate,
                initData.suggested_session_settings.adaptive_bitrate,
                adaptiveBitrateEnabled,
            )
            : adaptiveBitrateEnabled;
        if (lastSettingsRef.current.adaptive_bitrate === resolvedAdaptiveBitrate) return;
        const updated: DeskConfigSubmission = {
            ...lastSettingsRef.current,
            adaptive_bitrate: resolvedAdaptiveBitrate,
        };
        applyRemoteSettings(updated);
    }, [adaptiveBitrateEnabled, isRTCConnected, deskId, initData, applyRemoteSettings]);

    const handleConfigCancel = () => {
        setIsConfigOpen(false);
        navigate(`/desk/${deskId}`);
    };

    const requestRemoteControl = useCallback((clipboardIntent?: boolean) => {
        if (!deskId) return;
        // In a restricted session, only auto-request the capabilities the code's
        // ceiling does not deny; an owner session leaves every dimension visible so
        // this keeps the previous unconditional behaviour.
        const wantClipboard = restricted.capabilityVisible('allow_clipboard_sync')
            && (clipboardIntent ?? true);
        const wantFileTransfer = restricted.capabilityVisible('allow_file_transfer');
        const requestControlData = {
            accept_clipboard_sync: wantClipboard, // Auto-request clipboard when the ceiling allows it
            accept_file_transfer: wantFileTransfer,
        };
        console.log(`Sending REQUIRE_CONTROL signaling, requestControlData:`, requestControlData);
        const requestId = sendMessage(
            SIGNALING_TYPE_CODE_REQUIRE_CONTROL,
            requestControlData,
            deskId,
        );
        controlRequestRef.current = { requestId, kind: "require", wantsClipboard: wantClipboard };
        setIsWaitingApproval(true);
    }, [deskId, restricted, sendMessage]);

    const handleRequestControl = () => {
        if (hasControl) {
            releaseInputsRef.current();
            // Clear the host overlay while this connection still owns control;
            // the data-channel route is denied immediately after ReleaseControl.
            deactivateWhiteboardRef.current();
            const requestId = sendMessage(
                SIGNALING_TYPE_CODE_RELEASE_CONTROL,
                null,
                deskId,
            );
            controlRequestRef.current = { requestId, kind: "release" };
            setIsWaitingApproval(true);
            return;
        }
        requestRemoteControl();
    };

    useEffect(() => {
        const claimed = claimControlReconnect(
            controlReconnectRef.current,
            initData?.connection_epoch,
            isRTCConnected,
        );
        controlReconnectRef.current = claimed.intent;
        if (claimed.shouldRequest) {
            const clipboardIntent = clipboardReconnectIntentRef.current;
            clipboardReconnectIntentRef.current = null;
            requestRemoteControl(clipboardIntent ?? undefined);
        }
    }, [initData?.connection_epoch, isRTCConnected, requestRemoteControl]);

    const handleDisconnect = () => {
        const finalEpoch = initData?.connection_epoch
            ?? settingsReconnectRef.current?.previousEpoch;
        settingsReconnectRef.current = null;
        controlReconnectRef.current = null;
        clipboardReconnectIntentRef.current = null;
        clearPrivateScreenPending();
        releaseInputsRef.current();
        deactivateWhiteboardRef.current();
        if (deskId) {
            if (isPrivateScreen) {
                console.log(`Disabling private screen before disconnect`);
                sendMessage(SIGNALING_TYPE_CODE_SET_PRIVATE_SCREEN_VISIBILITY, { visible: false }, deskId);
            }
            if (finalEpoch) {
                sendMessage(
                    SIGNALING_TYPE_CODE_CLOSE_REMOTE_SESSION,
                    {
                        connection_epoch: finalEpoch,
                        finalize_logical_connection: true,
                    },
                    deskId,
                );
            }
        }
        closeRTC();
        navigate(`/desk/${deskId}`);
    };

    const handleMediaPipelineRetry = () => {
        if (!deskId || mediaRetryPending) return;
        beginVideoTransition();
        setMediaRetryPending(true);
        mediaRetryRequestIdRef.current = sendMessage(
            SIGNALING_TYPE_CODE_RETRY_MEDIA_PIPELINE,
            { connection_epoch: initData?.connection_epoch },
            deskId,
        );
    };

    const handleTogglePrivateScreen = () => {
        if (!deskId) return;
        const newState = !isPrivateScreen;
        if (isPrivateScreenPending) return;
        console.log(`Toggling private screen: ${newState}`);
        const requestId = sendMessage(
            SIGNALING_TYPE_CODE_SET_PRIVATE_SCREEN_VISIBILITY,
            { visible: newState },
            deskId,
        );
        startPrivateScreenPending(newState, requestId);
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
                preferences={devicePreferences}
                onSubmit={handleConfigSubmit}
                onCancel={handleConfigCancel}
                adaptiveQualityEnabled={adaptiveQualityEnabled}
                onAdaptiveQualityChange={setAdaptiveQualityEnabled}
                adaptiveBitrateEnabled={adaptiveBitrateEnabled}
                onAdaptiveBitrateChange={setAdaptiveBitrateEnabled}
                systemAudioAllowed={restricted.capabilityVisible('allow_system_audio_capture')}
            />
            <div className="flex items-center justify-between border-b p-4">
                <h2 className="text-lg font-semibold">{t('pages.desk.title')} - {deskId}</h2>
                <div className="flex items-center gap-4">
                    {hasControl && (
                        <div className="text-sm font-medium text-blue-500">{t('pages.desk.status.controlling')}</div>
                    )}
                    {systemAudioState && (
                        <div className="text-sm text-muted-foreground">
                            {t('pages.desk.systemAudioState', {
                                state: t(systemAudioStateTranslationKey(systemAudioState.state)),
                            })}
                        </div>
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
                            onCanPlay={() => {
                                setIsVideoReady(true);
                                if (!settingsApplyRef.current && !mediaRetryRequestIdRef.current) {
                                    finishVideoTransition();
                                }
                            }}
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
                            isActive={whiteboard.isInteractive}
                            videoRef={videoRef}
                            onPointerDown={whiteboard.handlePointerDown}
                            onPointerMove={whiteboard.handlePointerMove}
                            onPointerUp={whiteboard.handlePointerUp}
                            onClick={whiteboard.handleCanvasClick}
                        />

                        {/* Whiteboard toolbar */}
                        {whiteboard.isInteractive && (
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
                            className={`videoPlaceholder ${isVideoReady && !isVideoTransitioning ? 'hidden' : ''}`}
                            onContextMenu={(e) => { e.preventDefault() }}
                        >
                            <div className="placeholderContent">
                                <span className="artText">LCXL Remote Desk</span>
                                <div className="videoLoadingBar" aria-hidden="true" />
                                <p className="videoLoadingText" aria-live="polite">
                                    {t(isVideoTransitioning
                                        ? 'pages.desk.applyingVideoSettings'
                                        : 'pages.desk.connectingVideo')}
                                </p>
                                {!initData && (
                                    <p className="mt-1 text-sm text-white/60">
                                        {t('pages.desk.waitingPermission')}
                                    </p>
                                )}
                            </div>
                        </div>

                        {shouldShowMediaPipelineOverlay(
                            mediaPipelineState != null,
                            isConfigOpen,
                        ) && mediaPipelineState && (
                            <div className="absolute inset-0 z-[55] flex items-center justify-center bg-black/80 px-6 text-white backdrop-blur-sm">
                                <div className="flex max-w-lg flex-col items-center gap-4 rounded-xl border border-amber-300/40 bg-zinc-950/90 p-6 text-center shadow-2xl">
                                    <AlertTriangle className="h-10 w-10 text-amber-400" />
                                    <div>
                                        <h2 className="text-lg font-semibold">
                                            {t(mediaPipelineState.phase === "blocked"
                                                ? "pages.desk.mediaPipeline.blockedTitle"
                                                : "pages.desk.mediaPipeline.failedTitle")}
                                        </h2>
                                        <p className="mt-2 text-sm text-zinc-300">
                                            {deskErrorMessage(
                                                t,
                                                MEDIA_PIPELINE_ERROR_KEYS,
                                                mediaPipelineState.reason_code,
                                                mediaPipelineState.message,
                                                t("pages.desk.mediaPipeline.blockedDescription"),
                                            )}
                                        </p>
                                        {mediaPipelineState.reason_code != null && (
                                            <p className="mt-2 text-xs text-zinc-400">
                                                {t("pages.desk.mediaPipeline.reasonCode", {
                                                    code: mediaPipelineState.reason_code,
                                                })}
                                            </p>
                                        )}
                                        {mediaPipelineState.source_resolution && (
                                            <p className="mt-2 text-xs text-zinc-400">
                                                {t("pages.desk.mediaPipeline.sourceResolution", {
                                                    width: mediaPipelineState.source_resolution.width,
                                                    height: mediaPipelineState.source_resolution.height,
                                                })}
                                            </p>
                                        )}
                                        {!!mediaPipelineState.compatible_encoders?.length && (
                                            <p className="mt-1 text-xs text-zinc-400">
                                                {t("pages.desk.mediaPipeline.compatibleEncoders", {
                                                    encoders: mediaPipelineState.compatible_encoders.join(", "),
                                                })}
                                            </p>
                                        )}
                                    </div>
                                    <div className="flex gap-3">
                                        <Button variant="secondary" onClick={() => setIsConfigOpen(true)}>
                                            {t("pages.desk.mediaPipeline.chooseEncoder")}
                                        </Button>
                                        <Button onClick={handleMediaPipelineRetry} disabled={mediaRetryPending}>
                                            {mediaRetryPending && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                                            {t("pages.desk.mediaPipeline.retry")}
                                        </Button>
                                    </div>
                                </div>
                            </div>
                        )}

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
                                macKeyboardMappingController={macKeyboardMappingController}
                                whiteboard={whiteboard}
                            />
                        )}
                    </div>
                </TooltipProvider>
            </div >
        </div >
    )
}
