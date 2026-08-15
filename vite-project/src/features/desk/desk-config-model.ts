import type {
    AudioEncoderId,
    DisplayInfo,
    OperationSystemEnum,
    RemoteAccessInitializedData,
    RemoteSessionSettings,
    SelectedAudioDevice,
    VideoEncoderId,
} from "@/services/types"
import type { DeskDevicePreferencesV1 } from "./desk-preferences"

export type DeskConfigFormSettings = {
    enable_audio: boolean
    adaptive_web_page_resolution: boolean
    audio_capture: string | null
    audio_device: SelectedAudioDevice | null
    audio_encoder: AudioEncoderId | null
    enable_dirty_rect: boolean
    image_capture: string
    show_mouse: boolean
    video_device_name: string
    video_encoder: VideoEncoderId | null
    video_fps?: number
    video_quality: number
    wayland_control_mode: "auto" | "none" | "uinput" | "portal"
}

export type DeskConfigSubmission = RemoteSessionSettings & {
    adaptive_web_page_resolution: boolean
    wayland_control_mode: DeskConfigFormSettings["wayland_control_mode"]
}

export const DESK_CONFIG_DEFAULTS: DeskConfigFormSettings = {
    adaptive_web_page_resolution: true,
    audio_capture: null,
    audio_device: null,
    audio_encoder: null,
    enable_audio: true,
    enable_dirty_rect: true,
    image_capture: "",
    show_mouse: true,
    video_device_name: "",
    video_encoder: null,
    video_fps: undefined,
    video_quality: 22,
    wayland_control_mode: "auto",
}

/** Host suggestions remain authoritative until this browser has saved a choice. */
export function preferSavedDeskValue<T>(
    preferences: DeskDevicePreferencesV1 | null,
    saved: T,
    suggested: T,
): T {
    return preferences === null ? suggested : saved
}

export function formatDisplayLabel(device: DisplayInfo): string {
    const name = device.display_device_name ?? device.device_name
    const coordinates = device.desktop_coordinates
    const width = coordinates.right - coordinates.left
    const height = coordinates.bottom - coordinates.top
    return `${name} (${width}x${height})`
}

export function canEnableAdaptiveResolution(
    selectedDeviceName: string | undefined | null,
    virtualDisplayDeviceName: string | undefined | null,
): boolean {
    return (
        !!virtualDisplayDeviceName
        && selectedDeviceName === virtualDisplayDeviceName
    )
}

export function hasNoDisplaysForMode(
    selectedImageCapture: string | undefined | null,
    videoDeviceList: ReadonlyArray<DisplayInfo> | undefined | null,
): boolean {
    return (
        !!selectedImageCapture
        && (!videoDeviceList || videoDeviceList.length === 0)
    )
}

export function shouldShowNoDisplayWarning(
    captureUnavailable: boolean,
    noDisplaysForMode: boolean,
): boolean {
    return noDisplaysForMode && !captureUnavailable
}

export function shouldShowAdminPrivilegeWarning(
    isAdmin: boolean | undefined,
    operationSystem: OperationSystemEnum | undefined,
): boolean {
    // macOS desktop hosts are expected to run in the signed-in user's
    // session. Their capabilities are controlled by TCC permissions rather
    // than by running the server as root, so the generic admin warning does
    // not apply there.
    return isAdmin === false && operationSystem !== "Mac"
}

export function pickDefaultDeviceName(
    devices: ReadonlyArray<DisplayInfo>,
): string {
    if (devices.length === 0) return ""

    const primary = devices.find((device) => {
        const coordinates = device.desktop_coordinates
        return coordinates.left === 0 && coordinates.top === 0
    })
    return (primary ?? devices[0]).device_name ?? ""
}

const CAPTURE_PREFERRED_ORDER = [
    "WGC",
    "DXGI",
    "GDI",
    "WAYLANDPORTAL",
    "X11",
] as const

export function orderCaptureModes(modes: ReadonlyArray<string>): string[] {
    return [
        ...CAPTURE_PREFERRED_ORDER.filter((mode) => modes.includes(mode)),
        ...modes
            .filter((mode) => !CAPTURE_PREFERRED_ORDER.includes(
                mode as typeof CAPTURE_PREFERRED_ORDER[number],
            ))
            .sort(),
    ]
}

export interface CaptureTargetNormalization {
    effectiveMode: string
    effectiveDeviceName: string
    staleMode: string | null
    staleDevice: string | null
    hasUsableCaptureTarget: boolean
}

export type VideoDeviceMap = Readonly<Record<string, ReadonlyArray<DisplayInfo>>>

export function normalizeCaptureTarget(
    savedMode: string | undefined | null,
    savedDeviceName: string | undefined | null,
    videoDeviceList: VideoDeviceMap | undefined | null,
): CaptureTargetNormalization {
    const devicesByMode = videoDeviceList ?? {}
    const usableModes = orderCaptureModes(Object.keys(devicesByMode))
        .filter((mode) => (devicesByMode[mode]?.length ?? 0) > 0)
    const requestedMode = savedMode ?? ""
    const effectiveMode = requestedMode
        && usableModes.includes(requestedMode)
        ? requestedMode
        : usableModes[0] ?? ""
    const devices = effectiveMode ? devicesByMode[effectiveMode] ?? [] : []
    const requestedDevice = savedDeviceName ?? ""
    const effectiveDeviceName = requestedDevice
        && devices.some((device) => device.device_name === requestedDevice)
        ? requestedDevice
        : pickDefaultDeviceName(devices)

    return {
        effectiveMode,
        effectiveDeviceName,
        staleMode: requestedMode && requestedMode !== effectiveMode
            ? requestedMode
            : null,
        staleDevice: requestedDevice && requestedDevice !== effectiveDeviceName
            ? requestedDevice
            : null,
        hasUsableCaptureTarget: !!effectiveMode && !!effectiveDeviceName,
    }
}

export function canConnectCaptureTarget(
    mode: string | undefined | null,
    deviceName: string | undefined | null,
    videoDeviceList: VideoDeviceMap | undefined | null,
): boolean {
    if (!mode || !deviceName || !videoDeviceList) return false
    return (videoDeviceList[mode] ?? [])
        .some((device) => device.device_name === deviceName)
}

function videoEncoderId(value: string | null | undefined): VideoEncoderId | null {
    switch (value?.toUpperCase()) {
        case "X264": return "X264"
        case "H264":
        case "OPENH264": return "OpenH264"
        case "VP8": return "VP8"
        case "VP9": return "VP9"
        case "AV1": return "AV1"
        default: return null
    }
}

export function resolveVideoEncoder(
    preferred: string | null | undefined,
    suggested: string | null | undefined,
    available: ReadonlyArray<string>,
): VideoEncoderId | null {
    const availableIds = new Set(available.map(videoEncoderId).filter(
        (value): value is VideoEncoderId => value !== null,
    ))
    for (const candidate of [videoEncoderId(preferred), videoEncoderId(suggested)]) {
        if (candidate && availableIds.has(candidate)) return candidate
    }
    return available.map(videoEncoderId).find(
        (value): value is VideoEncoderId => value !== null,
    ) ?? null
}

export function resolveAudioEncoder(
    preferred: string | null | undefined,
    suggested: string | null | undefined,
    available: ReadonlyArray<string>,
): AudioEncoderId | null {
    const candidates = [preferred, suggested, ...available]
    return candidates.some((value) => value?.toLowerCase() === "opus")
        && available.some((value) => value.toLowerCase() === "opus")
        ? "Opus"
        : null
}

/**
 * Convert nullable "auto" form values into a complete wire configuration and
 * pin host-unsupported fields to the host suggestion. This keeps the browser
 * controller capability-driven (not OS-name-driven), including Android hosts
 * whose FPS/quality/capture source are fixed.
 */
export function resolveExecutableDeskConfig(
    values: DeskConfigFormSettings,
    initData: RemoteAccessInitializedData,
): DeskConfigFormSettings {
    const suggested = initData.suggested_session_settings
    const capabilities = initData.session_settings_capabilities
    const fixed = <K extends keyof typeof capabilities>(key: K) => (
        capabilities[key] === "unsupported"
    )
    const resolved = { ...values }

    if (fixed("image_capture") && suggested.image_capture) {
        resolved.image_capture = suggested.image_capture
    }
    if (fixed("video_device_name") && suggested.video_device_name) {
        resolved.video_device_name = suggested.video_device_name
    }
    if (fixed("show_mouse")) resolved.show_mouse = suggested.show_mouse
    if (fixed("video_quality")) resolved.video_quality = suggested.video_quality
    if (fixed("video_fps")) resolved.video_fps = suggested.video_fps
    if (fixed("enable_dirty_rect")) {
        resolved.enable_dirty_rect = suggested.enable_dirty_rect
    }
    if (fixed("capture_audio")) resolved.enable_audio = suggested.capture_audio

    resolved.video_encoder = resolveVideoEncoder(
        fixed("video_encoder") ? null : resolved.video_encoder,
        suggested.video_encoder,
        initData.video_encoder_list,
    )

    if (resolved.enable_audio) {
        const captureModes = Object.keys(initData.audio_device_list ?? {})
        const preferredCapture = fixed("audio_capture")
            ? suggested.audio_capture
            : resolved.audio_capture
        resolved.audio_capture = preferredCapture
            && captureModes.includes(preferredCapture)
            ? preferredCapture
            : suggested.audio_capture
                && captureModes.includes(suggested.audio_capture)
                ? suggested.audio_capture
                : captureModes[0] ?? null
        const devices = resolved.audio_capture
            ? initData.audio_device_list[resolved.audio_capture] ?? []
            : []
        const preferredDevice = fixed("audio_device")
            ? suggested.audio_device
            : resolved.audio_device
        const fallbackDevice = devices.find((device) => device.default) ?? devices[0]
        resolved.audio_device = preferredDevice && devices.some((device) => (
            device.data_flow === preferredDevice.audio_data_flow
            && (preferredDevice.audio_device_id === null
                || device.id === preferredDevice.audio_device_id)
        ))
            ? preferredDevice
            : suggested.audio_device && devices.some((device) => (
                device.data_flow === suggested.audio_device?.audio_data_flow
                && (suggested.audio_device.audio_device_id === null
                    || device.id === suggested.audio_device.audio_device_id)
            ))
                ? suggested.audio_device
                : fallbackDevice
                    ? {
                        audio_data_flow: fallbackDevice.data_flow,
                        audio_device_id: null,
                    }
                    : null
        resolved.audio_encoder = resolveAudioEncoder(
            fixed("audio_encoder") ? null : resolved.audio_encoder,
            suggested.audio_encoder,
            initData.audio_encoder_list,
        )
    }

    return resolved
}

export function toRemoteSessionSettings(
    values: DeskConfigFormSettings,
    adaptiveBitrate: boolean,
): DeskConfigSubmission {
    const videoFps = values.video_fps == null
        ? 60
        : Number(values.video_fps)

    if (!values.image_capture || !values.video_device_name || !values.video_encoder) {
        throw new Error("incomplete executable video settings")
    }
    if (values.enable_audio
        && (!values.audio_capture || !values.audio_device || !values.audio_encoder)) {
        throw new Error("incomplete executable audio settings")
    }

    return {
        adaptive_bitrate: adaptiveBitrate,
        audio: values.enable_audio ? {
            audio_capture: values.audio_capture!,
            audio_device: values.audio_device!,
            audio_encoder: values.audio_encoder!,
        } : null,
        enable_dirty_rect: values.enable_dirty_rect,
        image_capture: values.image_capture,
        show_mouse: values.show_mouse,
        video_device_name: values.video_device_name,
        video_encoder: values.video_encoder,
        video_fps: videoFps > 0 ? videoFps : 60,
        video_quality: Number(values.video_quality),
        adaptive_web_page_resolution: values.adaptive_web_page_resolution,
        wayland_control_mode: values.wayland_control_mode,
    }
}

export function toDeskDevicePreferences(
    values: DeskConfigFormSettings,
): DeskDevicePreferencesV1 {
    const explicitAudioDevice = values.audio_device?.audio_device_id?.trim()
    return {
        version: 1,
        captureAudio: values.enable_audio,
        imageCapture: values.image_capture || null,
        videoDeviceName: values.video_device_name || null,
        showMouse: values.show_mouse ?? true,
        adaptiveWebPageResolution:
            values.adaptive_web_page_resolution ?? true,
        videoEncoder: values.video_encoder ?? null,
        videoQuality: Number(values.video_quality),
        videoFps: values.video_fps == null || Number(values.video_fps) <= 0
            ? null
            : Number(values.video_fps),
        enableDirtyRect: values.enable_dirty_rect ?? true,
        audioCapture: values.audio_capture ?? null,
        audioDevice: explicitAudioDevice && values.audio_device
            ? {
                audioDataFlow: values.audio_device.audio_data_flow,
                audioDeviceId: explicitAudioDevice,
            }
            : null,
        audioEncoder: values.audio_encoder ?? null,
        waylandControlMode:
            (values.wayland_control_mode as DeskDevicePreferencesV1["waylandControlMode"])
            ?? "auto",
    }
}
