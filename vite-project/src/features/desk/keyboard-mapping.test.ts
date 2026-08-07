import { describe, expect, it } from "vitest"
import { getMacKeyboardMappingController } from "./keyboard-mapping"

describe("getMacKeyboardMappingController", () => {
    it("enables the compatibility mapping for Windows → macOS", () => {
        expect(getMacKeyboardMappingController("Mac", {
            platform: "Win32",
            userAgent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64)",
        })).toBe("Windows")
    })

    it("enables the compatibility mapping for Linux → macOS", () => {
        expect(getMacKeyboardMappingController("Mac", {
            platform: "Linux x86_64",
            userAgent: "Mozilla/5.0 (X11; Linux x86_64)",
        })).toBe("Linux")
    })

    it("prefers User-Agent Client Hints when available", () => {
        expect(getMacKeyboardMappingController("Mac", {
            platform: "",
            userAgent: "Mozilla/5.0",
            userAgentData: { platform: "Linux" },
        })).toBe("Linux")
    })

    it("does not remap other remote operating systems", () => {
        expect(getMacKeyboardMappingController("Windows", {
            platform: "Win32",
            userAgent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64)",
        })).toBeUndefined()
    })

    it("does not remap a macOS controller", () => {
        expect(getMacKeyboardMappingController("Mac", {
            platform: "MacIntel",
            userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X)",
        })).toBeUndefined()
    })

    it("does not mistake Android for a Linux desktop controller", () => {
        expect(getMacKeyboardMappingController("Mac", {
            platform: "Linux armv8l",
            userAgent: "Mozilla/5.0 (Linux; Android 15)",
        })).toBeUndefined()
    })
})
