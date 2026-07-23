import { afterEach, describe, expect, it, vi } from "vitest"

import { isTauriShell, openExternalUrl } from "./open-external"

// jsdom's `window.location.assign` is not directly spyable, so each test swaps
// in a stub location object (with a controllable `search`) and restores the
// original afterwards.
const originalLocation = window.location

function stubLocation(search: string): ReturnType<typeof vi.fn> {
    const assign = vi.fn()
    Object.defineProperty(window, "location", {
        configurable: true,
        value: { ...originalLocation, search, assign },
    })
    return assign
}

afterEach(() => {
    vi.restoreAllMocks()
    sessionStorage.clear()
    Object.defineProperty(window, "location", {
        configurable: true,
        value: originalLocation,
    })
})

describe("isTauriShell", () => {
    it("persists the first-frame marker across SPA query changes", () => {
        stubLocation("?tauri=1")
        expect(isTauriShell()).toBe(true)
        stubLocation("?foo=bar")
        expect(isTauriShell()).toBe(true)
        sessionStorage.clear()
        stubLocation("")
        expect(isTauriShell()).toBe(false)
    })
})

describe("openExternalUrl", () => {
    it("in the Tauri shell, navigates top-level (no popup) so on_navigation routes to the OS browser", () => {
        const assignSpy = stubLocation("?tauri=1")
        const openSpy = vi.spyOn(window, "open").mockReturnValue(null)

        openExternalUrl("http://192.168.50.50/console/")

        // The window.open popup is exactly what showed the http interstitial, so
        // it must not be used inside the shell.
        expect(openSpy).not.toHaveBeenCalled()
        expect(assignSpy).toHaveBeenCalledWith("http://192.168.50.50/console/")
    })

    it("in a normal browser, opens a new tab and does not navigate away", () => {
        const assignSpy = stubLocation("")
        const openSpy = vi.spyOn(window, "open").mockReturnValue({} as Window)

        openExternalUrl("https://lcxbox.app/console/")

        expect(openSpy).toHaveBeenCalledWith(
            "https://lcxbox.app/console/",
            "_blank",
            "noopener",
        )
        expect(assignSpy).not.toHaveBeenCalled()
    })

    it("in a normal browser, falls back to a top-level navigation when the popup is blocked", () => {
        const assignSpy = stubLocation("")
        vi.spyOn(window, "open").mockReturnValue(null)

        openExternalUrl("https://lcxbox.app/console/")

        expect(assignSpy).toHaveBeenCalledWith("https://lcxbox.app/console/")
    })
})
