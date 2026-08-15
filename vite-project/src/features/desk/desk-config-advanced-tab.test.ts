import { describe, expect, it } from "vitest"

import {
    canonicalVideoEncoderOptions,
    encoderIdForSetting,
    limitsAcceptResolution,
    shouldShowWaylandControlMode,
} from "./desk-config-advanced-tab"

const openH264Limits = {
    max_landscape: { width: 3840, height: 2160 },
    max_portrait: { width: 2160, height: 3840 },
    width_alignment: 2,
    height_alignment: 2,
}

describe("encoder resolution filtering", () => {
    it("keeps concrete H.264 implementations distinct", () => {
        expect(encoderIdForSetting("X264")).toBe("X264")
        expect(encoderIdForSetting("H264")).toBe("OpenH264")
    })

    it("uses canonical encoder ids as select values", () => {
        expect(canonicalVideoEncoderOptions(["H264", "X264", "VP8"])).toEqual([
            { id: "OpenH264", settingName: "H264" },
            { id: "X264", settingName: "X264" },
            { id: "VP8", settingName: "VP8" },
        ])
    })

    it("deduplicates legacy and canonical OpenH264 spellings", () => {
        expect(canonicalVideoEncoderOptions(["H264", "OpenH264"])).toEqual([
            { id: "OpenH264", settingName: "H264" },
        ])
    })

    it("accepts UHD but rejects DCI 4K for OpenH264", () => {
        expect(limitsAcceptResolution(openH264Limits, { width: 3840, height: 2160 })).toBe(true)
        expect(limitsAcceptResolution(openH264Limits, { width: 4096, height: 2160 })).toBe(false)
    })

    it("uses portrait limits and rejects misaligned input", () => {
        expect(limitsAcceptResolution(openH264Limits, { width: 2160, height: 3840 })).toBe(true)
        expect(limitsAcceptResolution(openH264Limits, { width: 2159, height: 3840 })).toBe(false)
    })
})

describe("Wayland control mode visibility", () => {
    it("shows the setting only for Linux hosts", () => {
        expect(shouldShowWaylandControlMode("Linux")).toBe(true)
        expect(shouldShowWaylandControlMode("Mac")).toBe(false)
        expect(shouldShowWaylandControlMode("Windows")).toBe(false)
        expect(shouldShowWaylandControlMode(undefined)).toBe(false)
    })
})
