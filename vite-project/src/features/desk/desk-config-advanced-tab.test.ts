import { describe, expect, it } from "vitest"

import {
    encoderIdForSetting,
    limitsAcceptResolution,
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

    it("accepts UHD but rejects DCI 4K for OpenH264", () => {
        expect(limitsAcceptResolution(openH264Limits, { width: 3840, height: 2160 })).toBe(true)
        expect(limitsAcceptResolution(openH264Limits, { width: 4096, height: 2160 })).toBe(false)
    })

    it("uses portrait limits and rejects misaligned input", () => {
        expect(limitsAcceptResolution(openH264Limits, { width: 2160, height: 3840 })).toBe(true)
        expect(limitsAcceptResolution(openH264Limits, { width: 2159, height: 3840 })).toBe(false)
    })
})
