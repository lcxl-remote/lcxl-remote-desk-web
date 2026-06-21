// Thin, defensive wrappers around the Keyboard Lock API
// (https://developer.mozilla.org/en-US/docs/Web/API/Keyboard/lock).
//
// While the desk view is in native fullscreen we lock the Escape key so that:
//   1. Escape key events reach the page and are forwarded to the host (a plain
//      fullscreen swallows Escape to exit, so the host never sees it).
//   2. Exiting fullscreen requires press-and-hold instead of a single tap.
//
// The API is Chromium-only and requires a secure context, so every call is
// guarded and failures degrade gracefully (the page just behaves as before).

type KeyboardLockApi = {
    lock?: (keyCodes?: string[]) => Promise<void>;
    unlock?: () => void;
};

export type KeyboardLockNavigator = { keyboard?: KeyboardLockApi };

/**
 * Capture the Escape key for the page while in fullscreen. No-op (returns
 * false) when the Keyboard Lock API is unavailable or the request is rejected.
 * Must be called once the document is actually in fullscreen.
 */
export async function lockEscapeKey(
    nav: KeyboardLockNavigator = navigator as unknown as KeyboardLockNavigator,
): Promise<boolean> {
    const keyboard = nav.keyboard;
    if (!keyboard?.lock) return false;
    try {
        await keyboard.lock(["Escape"]);
        return true;
    } catch {
        // Rejected (e.g. not in fullscreen, insecure context) — leave the
        // default fullscreen Escape behaviour in place.
        return false;
    }
}

/** Release any active keyboard lock. Safe to call unconditionally. */
export function unlockKeyboard(
    nav: KeyboardLockNavigator = navigator as unknown as KeyboardLockNavigator,
): void {
    try {
        nav.keyboard?.unlock?.();
    } catch {
        // Ignore — nothing was locked, or the API is unavailable.
    }
}
