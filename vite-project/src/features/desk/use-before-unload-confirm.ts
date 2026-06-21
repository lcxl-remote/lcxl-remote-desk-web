import { useEffect } from "react";

/**
 * Prompt the browser's native "Leave site? / Reload site?" confirmation while
 * `enabled` is true, guarding against accidentally closing, reloading or
 * navigating away from an active remote-desktop session.
 *
 * Browsers ignore any custom message and show a generic prompt; calling
 * `preventDefault()` and setting `returnValue` is what triggers it. The dialog
 * only appears in response to a genuine user gesture on the page, so it is a
 * no-op (no spurious prompts) when the user has not interacted.
 */
export function useBeforeUnloadConfirm(enabled: boolean) {
    useEffect(() => {
        if (!enabled) return;
        const handleBeforeUnload = (event: BeforeUnloadEvent) => {
            event.preventDefault();
            // Legacy assignment still required by some browsers to trigger the
            // confirmation dialog.
            event.returnValue = "";
        };
        window.addEventListener("beforeunload", handleBeforeUnload);
        return () => window.removeEventListener("beforeunload", handleBeforeUnload);
    }, [enabled]);
}
