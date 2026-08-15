import { describe, it, expect } from "vitest"
import {
    canConnectCaptureTarget,
    canEnableAdaptiveResolution,
    DESK_CONFIG_DEFAULTS,
    formatDisplayLabel,
    hasNoDisplaysForMode,
    normalizeCaptureTarget,
    orderCaptureModes,
    pickDefaultDeviceName,
    preferSavedDeskValue,
    resolveAudioEncoder,
    resolveExecutableDeskConfig,
    resolveVideoEncoder,
    shouldShowAdminPrivilegeWarning,
    shouldShowNoDisplayWarning,
    toDeskDevicePreferences,
    toRemoteSessionSettings,
} from "./desk-config-model"
import type { DisplayInfo, RemoteAccessInitializedData } from "@/services/types"
import { DEFAULT_DESK_DEVICE_PREFERENCES } from "./desk-preferences"

function makeDisplayInfo(
    overrides: Partial<DisplayInfo> & {
        desktop_coordinates: DisplayInfo["desktop_coordinates"]
    },
): DisplayInfo {
    return {
        device_name: overrides.device_name ?? "\\\\.\\DISPLAY1",
        display_device_name: overrides.display_device_name ?? null,
        attached_to_desktop: overrides.attached_to_desktop ?? true,
        rotation: overrides.rotation ?? 0,
        resolutions: overrides.resolutions ?? [],
        desktop_coordinates: overrides.desktop_coordinates,
    } as DisplayInfo
}

describe("formatDisplayLabel", () => {
    it("computes width and height from rect corners, not the raw (right, bottom) point", () => {
        // Primary at the origin: right/bottom coincidentally equals width/height,
        // so the bug is invisible here. Guard it anyway in case the helper drifts.
        const primary = makeDisplayInfo({
            device_name: "\\\\.\\DISPLAY1",
            desktop_coordinates: { left: 0, top: 0, right: 1280, bottom: 800 },
        })
        expect(formatDisplayLabel(primary)).toBe("\\\\.\\DISPLAY1 (1280x800)")
    })

    it("does not include the virtual desktop offset in the rendered resolution", () => {
        // An IDD attached to the right of a 1280-wide primary sits at
        // left=1280, right=2780. The old implementation showed
        // "2780x900" because it printed `right` / `bottom` directly;
        // the fix must show the real 1500x900 panel size.
        const idd = makeDisplayInfo({
            device_name: "\\\\.\\DISPLAY8",
            desktop_coordinates: { left: 1280, top: 0, right: 2780, bottom: 900 },
        })
        const label = formatDisplayLabel(idd)
        expect(label).toContain("1500x900")
        expect(label).not.toContain("2780x900")
    })

    it("handles a monitor positioned above or to the left of the primary (negative offsets)", () => {
        // Users can drag a second monitor to the left in Display
        // Settings, producing negative left/top. The width/height
        // arithmetic must still match the panel size.
        const leftSide = makeDisplayInfo({
            device_name: "\\\\.\\DISPLAY2",
            desktop_coordinates: { left: -1920, top: 0, right: 0, bottom: 1080 },
        })
        expect(formatDisplayLabel(leftSide)).toBe("\\\\.\\DISPLAY2 (1920x1080)")
    })

    it("prefers display_device_name over device_name when present", () => {
        const friendly = makeDisplayInfo({
            device_name: "\\\\.\\DISPLAY1",
            display_device_name: "Generic PnP Monitor",
            desktop_coordinates: { left: 0, top: 0, right: 1920, bottom: 1080 },
        })
        expect(formatDisplayLabel(friendly)).toBe(
            "Generic PnP Monitor (1920x1080)",
        )
    })

    it("falls back to device_name when display_device_name is null", () => {
        const noFriendly = makeDisplayInfo({
            device_name: "\\\\.\\DISPLAY8",
            display_device_name: null,
            desktop_coordinates: { left: 1280, top: 0, right: 2780, bottom: 900 },
        })
        expect(formatDisplayLabel(noFriendly)).toBe(
            "\\\\.\\DISPLAY8 (1500x900)",
        )
    })
})

describe("canEnableAdaptiveResolution", () => {
    it("enables when the selection matches the virtual display name", () => {
        expect(
            canEnableAdaptiveResolution("\\\\.\\DISPLAY8", "\\\\.\\DISPLAY8"),
        ).toBe(true)
    })

    it("disables when the user selected a physical display", () => {
        expect(
            canEnableAdaptiveResolution("\\\\.\\DISPLAY1", "\\\\.\\DISPLAY8"),
        ).toBe(false)
    })

    it("disables when no virtual display is attached (daemon reports None)", () => {
        // Even if the user happens to pick a sensible-looking device,
        // adaptive cannot fire because the daemon would reject it with
        // FEATURE_UNAVAILABLE. The dialog should reflect this state.
        expect(canEnableAdaptiveResolution("\\\\.\\DISPLAY1", null)).toBe(false)
        expect(canEnableAdaptiveResolution("\\\\.\\DISPLAY1", undefined)).toBe(
            false,
        )
    })

    it("disables when nothing is selected yet", () => {
        // Initial dialog load before the user picks any device — the
        // form state is empty string ("") and the toggle must default
        // to disabled regardless of whether an IDD is attached.
        expect(canEnableAdaptiveResolution("", "\\\\.\\DISPLAY8")).toBe(false)
        expect(canEnableAdaptiveResolution(undefined, "\\\\.\\DISPLAY8")).toBe(
            false,
        )
    })
})

describe("hasNoDisplaysForMode", () => {
    const display = makeDisplayInfo({
        desktop_coordinates: { left: 0, top: 0, right: 1280, bottom: 800 },
    })

    it("flags a chosen mode whose enumerated display list is empty", () => {
        // The headless / detached-session case: the WGC key exists so the mode
        // is offered, but EnumDisplayMonitors returned zero, so no picker can
        // render. Connect must be blocked.
        expect(hasNoDisplaysForMode("WGC", [])).toBe(true)
        expect(hasNoDisplaysForMode("WGC", null)).toBe(true)
        expect(hasNoDisplaysForMode("WGC", undefined)).toBe(true)
    })

    it("does not flag a mode that has at least one display", () => {
        expect(hasNoDisplaysForMode("WGC", [display])).toBe(false)
    })

    it("does not flag before any capture mode is selected", () => {
        // Nothing to gate yet — the empty list is just the no-selection state,
        // not a host-without-display state.
        expect(hasNoDisplaysForMode("", [])).toBe(false)
        expect(hasNoDisplaysForMode(undefined, [])).toBe(false)
        expect(hasNoDisplaysForMode(null, undefined)).toBe(false)
    })

    it("suppresses the mode warning when capture is globally unavailable", () => {
        expect(shouldShowNoDisplayWarning(true, true)).toBe(false)
        expect(shouldShowNoDisplayWarning(false, true)).toBe(true)
        expect(shouldShowNoDisplayWarning(false, false)).toBe(false)
    })
})

describe("shouldShowAdminPrivilegeWarning", () => {
    it("does not warn for a non-root macOS desktop host", () => {
        expect(shouldShowAdminPrivilegeWarning(false, "Mac")).toBe(false)
    })

    it("still warns for non-admin Windows, Linux, and legacy hosts", () => {
        expect(shouldShowAdminPrivilegeWarning(false, "Windows")).toBe(true)
        expect(shouldShowAdminPrivilegeWarning(false, "Linux")).toBe(true)
        expect(shouldShowAdminPrivilegeWarning(false, undefined)).toBe(true)
    })

    it("does not warn when the host has administrative privileges", () => {
        expect(shouldShowAdminPrivilegeWarning(true, "Windows")).toBe(false)
    })
})

describe("desk config normalization", () => {
    it("uses host suggestions until this browser has saved a preference", () => {
        expect(preferSavedDeskValue(null, 60, 30)).toBe(30)
        expect(preferSavedDeskValue(
            DEFAULT_DESK_DEVICE_PREFERENCES,
            60,
            30,
        )).toBe(60)
    })

    it("defaults system audio capture to enabled", () => {
        expect(DESK_CONFIG_DEFAULTS.enable_audio).toBe(true)
    })

    it("resolves nullable automatic encoders from host capabilities", () => {
        expect(resolveVideoEncoder(null, null, ["VP8", "X264"])).toBe("VP8")
        expect(resolveVideoEncoder("H264", null, ["OpenH264"])).toBe("OpenH264")
        expect(resolveAudioEncoder(null, null, ["Opus"])).toBe("Opus")
    })

    it("pins unsupported Android host fields to its suggestions", () => {
        const initData = {
            audio_device_list: {},
            audio_encoder_list: [],
            video_device_list: {
                default: [makeDisplayInfo({
                    device_name: "android-screen",
                    desktop_coordinates: { left: 0, top: 0, right: 1080, bottom: 2400 },
                })],
            },
            video_encoder_list: ["OpenH264", "VP8"],
            suggested_session_settings: {
                capture_audio: false,
                image_capture: "default",
                video_device_name: "android-screen",
                show_mouse: false,
                video_encoder: null,
                video_quality: 22,
                video_fps: 30,
                enable_dirty_rect: false,
                adaptive_bitrate: false,
                audio_capture: null,
                audio_device: null,
                audio_encoder: null,
            },
            session_settings_capabilities: {
                capture_audio: "unsupported",
                image_capture: "unsupported",
                video_device_name: "unsupported",
                show_mouse: "unsupported",
                video_encoder: "reconnect",
                video_quality: "unsupported",
                video_fps: "unsupported",
                enable_dirty_rect: "unsupported",
                adaptive_bitrate: "unsupported",
                audio_capture: "unsupported",
                audio_device: "unsupported",
                audio_encoder: "unsupported",
            },
        } as RemoteAccessInitializedData
        const resolved = resolveExecutableDeskConfig({
            ...DESK_CONFIG_DEFAULTS,
            enable_audio: true,
            image_capture: "WGC",
            video_device_name: "desktop-screen",
            show_mouse: true,
            video_encoder: null,
            video_quality: 40,
            video_fps: 60,
            enable_dirty_rect: true,
        }, initData)

        expect(resolved).toMatchObject({
            enable_audio: false,
            image_capture: "default",
            video_device_name: "android-screen",
            show_mouse: false,
            video_encoder: "OpenH264",
            video_quality: 22,
            video_fps: 30,
            enable_dirty_rect: false,
        })
    })

    it("orders preferred capture modes before other backends", () => {
        expect(orderCaptureModes(["GDI", "Custom", "WGC", "DXGI"])).toEqual([
            "WGC",
            "DXGI",
            "GDI",
            "Custom",
        ])
    })

    it("prefers the display at the virtual-desktop origin", () => {
        const secondary = makeDisplayInfo({
            desktop_coordinates: {
                bottom: 1080,
                left: 1920,
                right: 3840,
                top: 0,
            },
            device_name: "secondary",
        })
        const primary = makeDisplayInfo({
            desktop_coordinates: {
                bottom: 1080,
                left: 0,
                right: 1920,
                top: 0,
            },
            device_name: "primary",
        })

        expect(pickDefaultDeviceName([secondary, primary])).toBe("primary")
        expect(pickDefaultDeviceName([])).toBe("")
    })

    it("removes form-only fields and normalizes disabled audio", () => {
        const settings = toRemoteSessionSettings({
            ...DESK_CONFIG_DEFAULTS,
            image_capture: "WGC",
            video_device_name: "primary",
            video_encoder: "X264",
            audio_device: {
                audio_data_flow: "Render",
                audio_device_id: "speaker",
            },
            enable_audio: false,
            video_fps: 0,
        }, true)

        expect(settings.audio).toBeNull()
        expect(settings.video_fps).toBe(60)
        expect(settings).not.toHaveProperty("enable_audio")
    })

    it("normalizes the wire default device selector to one preference auto value", () => {
        const preferences = toDeskDevicePreferences({
            ...DESK_CONFIG_DEFAULTS,
            audio_capture: "WASAPI",
            audio_device: {
                audio_data_flow: "Render",
                audio_device_id: null,
            },
        })

        expect(preferences.captureAudio).toBe(true)
        expect(preferences.audioCapture).toBe("WASAPI")
        expect(preferences.audioDevice).toBeNull()
    })
})

describe("capture target normalization", () => {
    const primary = makeDisplayInfo({
        device_name: "primary",
        desktop_coordinates: { left: 0, top: 0, right: 1280, bottom: 800 },
    })
    const secondary = makeDisplayInfo({
        device_name: "secondary",
        desktop_coordinates: { left: 1280, top: 0, right: 2560, bottom: 800 },
    })

    it("selects the first usable mode and primary display for empty saved values", () => {
        const target = normalizeCaptureTarget("", "", {
            GDI: [],
            X11: [secondary, primary],
            WAYLANDPORTAL: [],
        })
        expect(target).toEqual({
            effectiveMode: "X11",
            effectiveDeviceName: "primary",
            staleMode: null,
            staleDevice: null,
            hasUsableCaptureTarget: true,
        })
    })

    it("uses explicit cross-platform ordering and skips empty preferred modes", () => {
        expect(orderCaptureModes(["X11", "WAYLANDPORTAL", "GDI"])).toEqual([
            "GDI",
            "WAYLANDPORTAL",
            "X11",
        ])
        expect(normalizeCaptureTarget("", "", {
            WGC: [],
            DXGI: [primary],
        }).effectiveMode).toBe("DXGI")
    })

    it("corrects a stale mode and device while preserving valid values", () => {
        const corrected = normalizeCaptureTarget("X11", "gone", {
            WAYLANDPORTAL: [primary],
        })
        expect(corrected.effectiveMode).toBe("WAYLANDPORTAL")
        expect(corrected.effectiveDeviceName).toBe("primary")
        expect(corrected.staleMode).toBe("X11")
        expect(corrected.staleDevice).toBe("gone")

        const preserved = normalizeCaptureTarget("WAYLANDPORTAL", "secondary", {
            WAYLANDPORTAL: [primary, secondary],
        })
        expect(preserved.effectiveDeviceName).toBe("secondary")
        expect(preserved.staleMode).toBeNull()
        expect(preserved.staleDevice).toBeNull()
    })

    it("disables connect when no usable target exists or the device is not in the mode", () => {
        expect(normalizeCaptureTarget("", "", {}).hasUsableCaptureTarget).toBe(false)
        expect(canConnectCaptureTarget("X11", "primary", { X11: [primary] })).toBe(true)
        expect(canConnectCaptureTarget("X11", "missing", { X11: [primary] })).toBe(false)
        expect(canConnectCaptureTarget("", "", { X11: [primary] })).toBe(false)
    })
})
