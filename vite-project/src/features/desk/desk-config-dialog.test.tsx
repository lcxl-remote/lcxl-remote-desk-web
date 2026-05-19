import { describe, it, expect } from "vitest"
import { formatDisplayLabel } from "./desk-config-dialog"
import type { DisplayInfo } from "@/services/types"

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
