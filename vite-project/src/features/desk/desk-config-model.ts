import type {
    DeskSettings,
    DisplayInfo,
} from "@/services/types"

export type DeskConfigFormSettings = DeskSettings & {
    enable_audio: boolean
}

export const DESK_CONFIG_DEFAULTS: DeskConfigFormSettings = {
    adaptive_web_page_resolution: true,
    audio_encoder: null,
    enable_audio: false,
    enable_dirty_rect: true,
    image_capture: "",
    show_mouse: true,
    video_device_name: "",
    video_encoder: null,
    video_fps: undefined,
    video_quality: 22,
    video_zoom_ratio: 100,
    wayland_control_mode: "auto",
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

const CAPTURE_PREFERRED_ORDER = ["WGC", "DXGI", "GDI"] as const

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

export function toDeskSettings(
    values: DeskConfigFormSettings,
): DeskSettings {
    const {
        enable_audio: enableAudio,
        ...settings
    } = values
    const videoFps = values.video_fps == null
        ? undefined
        : Number(values.video_fps)

    return {
        ...settings,
        audio_device: enableAudio ? values.audio_device : null,
        video_device_name: values.video_device_name ?? "",
        video_fps: videoFps && videoFps > 0 ? videoFps : undefined,
        video_quality: Number(values.video_quality),
        video_zoom_ratio: Number(values.video_zoom_ratio),
        wayland_control_mode: values.wayland_control_mode ?? "auto",
    }
}
