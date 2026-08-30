import type {
    RemoteSessionSettings,
    RequestRemoteModel,
    SessionSettingApplyMode,
} from "@/services/types"

export const SETTINGS_APPLY_TIMEOUT_MS = 50_000

/** Resolve a browser preference against the host's advertised capability. */
export function resolveAdaptiveBitrateForHost(
    capability: SessionSettingApplyMode,
    suggested: boolean,
    preferred: boolean,
): boolean {
    return capability === "unsupported" ? suggested : preferred
}

export interface SettingsApplyTimerOwner {
    requestId: string
    timer: number | null
}

/**
 * Arms (or replaces) the deadline owned by one correlated settings request.
 * The same helper is used first while a command is queued offline and again
 * after `onSent`, so a queued command cannot block the settings UI forever.
 */
export function armSettingsApplyTimeout(
    pending: SettingsApplyTimerOwner,
    requestId: string,
    onTimeout: () => void,
    timeoutMs = SETTINGS_APPLY_TIMEOUT_MS,
): boolean {
    if (pending.requestId !== requestId) return false
    if (pending.timer !== null) window.clearTimeout(pending.timer)
    pending.timer = window.setTimeout(onTimeout, timeoutMs)
    return true
}

const SYSTEM_AUDIO_STATE_KEYS: Record<string, string> = {
    off: "pages.desk.systemAudioStateValue.off",
    starting: "pages.desk.systemAudioStateValue.starting",
    active: "pages.desk.systemAudioStateValue.active",
    restarting: "pages.desk.systemAudioStateValue.restarting",
    denied: "pages.desk.systemAudioStateValue.denied",
    failed: "pages.desk.systemAudioStateValue.failed",
}

export function systemAudioStateTranslationKey(state: string): string {
    return SYSTEM_AUDIO_STATE_KEYS[state] ?? "pages.desk.systemAudioStateValue.unknown"
}

/**
 * Whether the desk config dialog should be (re)opened automatically.
 *
 * It opens for the initial settings pick (REMOTE_ACCESS_INITIALIZED arrived, no connect attempt yet)
 * and after a terminal ICE failure so the user can retry. It must not reopen on
 * a transient `disconnected`, because ICE can recover without user action.
 */
export function shouldOpenConfigDialog(args: {
    hasInitData: boolean
    isRTCConnected: boolean
    rtcFailed: boolean
    hasAttemptedConnect: boolean
}): boolean {
    if (!args.hasInitData || args.isRTCConnected) return false
    return !args.hasAttemptedConnect || args.rtcFailed
}

/**
 * The blocked-media warning owns the whole video surface. Hide it while the
 * settings dialog is open so the dialog and its portalled encoder picker are
 * not covered by the warning's higher stacking layer. The state is retained,
 * so cancelling the dialog shows the warning again.
 */
export function shouldShowMediaPipelineOverlay(
    hasPipelineState: boolean,
    isConfigOpen: boolean,
): boolean {
    return hasPipelineState && !isConfigOpen
}

/** A successful signaling response still carries response_state with code 0. */
export function isRemoteSettingsFailure(
    isProtocolError: boolean,
    errorCode: number | null | undefined,
): boolean {
    return isProtocolError || !!errorCode
}

export interface ControlReconnectIntent {
    previousEpoch: string
}

/**
 * Remember control intent across a settings-driven PeerConnection rebuild.
 * The old authorization is deliberately not carried into the new epoch; the
 * browser must issue a fresh RequireControl after the replacement PC connects.
 */
export function armControlReconnect(
    hadControl: boolean,
    previousEpoch: string,
): ControlReconnectIntent | null {
    return hadControl ? { previousEpoch } : null
}

/**
 * Consume a pending control intent only after both the session epoch changed
 * and the replacement PeerConnection reached Connected. This prevents the
 * still-connected old PC from receiving the restoration request.
 */
export function claimControlReconnect(
    intent: ControlReconnectIntent | null,
    currentEpoch: string | undefined,
    isRTCConnected: boolean,
): { intent: ControlReconnectIntent | null; shouldRequest: boolean } {
    if (
        !intent
        || !currentEpoch
        || currentEpoch === intent.previousEpoch
        || !isRTCConnected
    ) {
        return { intent, shouldRequest: false }
    }
    return { intent: null, shouldRequest: true }
}

/**
 * These are the video fields for which the host replaces the capture/encoder
 * pipeline (or renegotiates the RTP codec). Other video knobs are applied to
 * the current generation and should not cover a healthy picture with a
 * loading surface.
 */
export function videoSettingsMayInterrupt(
    previous: RemoteSessionSettings | null,
    requested: RemoteSessionSettings,
): boolean {
    if (!previous) return false
    return previous.image_capture !== requested.image_capture
        || previous.video_device_name !== requested.video_device_name
        || previous.video_encoder !== requested.video_encoder
}

/**
 * Wait for evidence that the video element presented another frame. WebRTC
 * implementations do not consistently emit `timeupdate` when a sender swaps
 * encoders on the same track, so combine the dedicated frame callback with a
 * playback-quality counter. There is deliberately no time-based success
 * fallback: hiding the loading surface without a presented frame would mask a
 * genuinely frozen WebRTC stream as a successful encoder switch.
 */
export function waitForVideoPresentation(
    video: HTMLVideoElement,
    onPresented: () => void,
): () => void {
    let settled = false
    let frameCallbackId: number | null = null
    let pollId: number | null = null
    const initialFrameCount = typeof video.getVideoPlaybackQuality === "function"
        ? video.getVideoPlaybackQuality().totalVideoFrames
        : null

    const cleanup = () => {
        if (frameCallbackId !== null && typeof video.cancelVideoFrameCallback === "function") {
            video.cancelVideoFrameCallback(frameCallbackId)
        }
        if (pollId !== null) window.clearInterval(pollId)
        video.removeEventListener("timeupdate", finish)
        frameCallbackId = null
        pollId = null
    }
    const finish = () => {
        if (settled) return
        settled = true
        cleanup()
        onPresented()
    }

    if (typeof video.requestVideoFrameCallback === "function") {
        frameCallbackId = video.requestVideoFrameCallback(finish)
    }
    if (initialFrameCount !== null) {
        pollId = window.setInterval(() => {
            if (video.getVideoPlaybackQuality().totalVideoFrames > initialFrameCount) finish()
        }, 100)
    }
    video.addEventListener("timeupdate", finish, { once: true })

    return () => {
        if (settled) return
        settled = true
        cleanup()
    }
}

type DesktopRequestRemotePayload = Pick<
    RequestRemoteModel,
    "purpose" | "grant_session_id" | "requested_wayland_control_mode" | "org_id"
> & {
    connection_id: string
    session_target_id?: string
}

export function buildDesktopRequestRemotePayload(
    connectionId: string,
    grantSessionId: string | null,
    requestedWaylandControlMode: string,
    orgId?: number,
    sessionTargetId?: string,
): DesktopRequestRemotePayload {
    return {
        connection_id: connectionId,
        purpose: "remote_desktop",
        requested_wayland_control_mode: requestedWaylandControlMode,
        ...(sessionTargetId ? { session_target_id: sessionTargetId } : {}),
        ...(grantSessionId ? { grant_session_id: grantSessionId } : {}),
        ...(grantSessionId == null && orgId != null ? { org_id: orgId } : {}),
    }
}

/**
 * The video quality the encoder is running at right now.
 *
 * Adaptive adjustments travel as the `UpdateAdaptiveVideoQuality` command,
 * which by contract has no correlated response — the host never echoes back a
 * new accepted baseline, so `lastSettings` keeps reporting the value agreed at
 * session start no matter how far the adaptive loop has stepped the encoder.
 * The override is the only record that it moved. The loop and the stats panel
 * must therefore read the same expression; when they drifted apart the panel
 * showed a frozen quality next to a climbing adjustment counter.
 *
 * Larger is worse: this is a QP/CRF-style knob, so degrading steps up and
 * recovering steps down.
 */
export function effectiveVideoQuality(
    override: number | null,
    baseline: number | null | undefined,
): number | null {
    return override ?? baseline ?? null
}
