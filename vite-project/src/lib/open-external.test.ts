import { afterEach, describe, expect, it, vi } from "vitest"

import { openExternalUrl } from "./open-external"

// jsdom's `window.location.assign` is not directly spyable, so each test swaps
// in a stub location object and restores the original afterwards.
const originalLocation = window.location

function stubLocationAssign(): ReturnType<typeof vi.fn> {
    const assign = vi.fn()
    Object.defineProperty(window, "location", {
        configurable: true,
        value: { ...originalLocation, assign },
    })
    return assign
}

describe("openExternalUrl", () => {
    afterEach(() => {
        vi.restoreAllMocks()
        Object.defineProperty(window, "location", {
            configurable: true,
            value: originalLocation,
        })
    })

    it("opens a new tab and does not navigate when window.open succeeds", () => {
        const openSpy = vi.spyOn(window, "open").mockReturnValue({} as Window)
        const assignSpy = stubLocationAssign()

        openExternalUrl("https://example.com")

        expect(openSpy).toHaveBeenCalledWith(
            "https://example.com",
            "_blank",
            "noopener",
        )
        expect(assignSpy).not.toHaveBeenCalled()
    })

    it("falls back to a top-level navigation when window.open is swallowed (Tauri)", () => {
        vi.spyOn(window, "open").mockReturnValue(null)
        const assignSpy = stubLocationAssign()

        openExternalUrl("https://example.com")

        expect(assignSpy).toHaveBeenCalledWith("https://example.com")
    })
})
