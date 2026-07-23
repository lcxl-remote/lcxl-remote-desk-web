/**
 * Open an external URL from the frontend.
 *
 * In a normal browser this opens a new tab so the current page (e.g. the
 * onboarding wizard) is preserved.
 *
 * Inside the Tauri webview the page is served from an external HTTP origin. There
 * `window.open` to another origin spawns a popup webview that loads the target
 * itself — showing an http "site is not secure" interstitial and bypassing the
 * shell's `on_navigation` hook. So in the Tauri shell we instead do a top-level
 * navigation, which `on_navigation` intercepts: it opens the URL in the OS
 * default browser and cancels the in-webview load, so the current page stays put.
 *
 * The first shell URL carries `tauri=1`; the marker is copied to sessionStorage
 * so SPA navigation cannot accidentally turn a shell page into browser mode.
 */
export function isTauriShell(): boolean {
    try {
        if (new URLSearchParams(window.location.search).get("tauri") === "1") {
            sessionStorage.setItem("lcxl.tauriShell", "1")
        }
        return sessionStorage.getItem("lcxl.tauriShell") === "1"
    } catch {
        return false
    }
}

export function openExternalUrl(url: string): void {
    if (isTauriShell()) {
        // Top-level navigation → the shell's on_navigation cancels it in-webview
        // and hands the URL to the OS browser. No popup, no http interstitial.
        window.location.assign(url)
        return
    }
    const opened = window.open(url, "_blank", "noopener")
    if (!opened) {
        window.location.assign(url)
    }
}
