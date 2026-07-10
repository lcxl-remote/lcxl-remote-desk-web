/**
 * Open an external URL from the frontend.
 *
 * In a normal browser this opens a new tab. Inside the Tauri webview the page is
 * served from an external HTTP origin, so `window.open` to another origin is
 * swallowed and returns null; we then fall back to a top-level navigation, which
 * the Tauri shell intercepts (`on_navigation`) and routes to the OS default
 * browser without actually leaving the current page.
 */
export function openExternalUrl(url: string): void {
    const opened = window.open(url, "_blank", "noopener")
    if (!opened) {
        window.location.assign(url)
    }
}
