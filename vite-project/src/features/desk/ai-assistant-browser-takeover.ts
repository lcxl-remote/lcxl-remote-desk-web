type BrowserTakeoverEntry = {
    capability: { capability_id: string };
    ready: boolean;
    reason?: string | null;
};

const BROWSER_CAPABILITY_IDS = new Set([
    'browser.page.open',
    'browser.page.navigate',
    'browser.page.snapshot',
    'browser.page.wait',
    'browser.form.fill',
    'browser.element.activate',
]);

const TAKEOVER_REASONS = new Set([
    'adapter_unavailable',
    'browser_approval_required',
    'browser_disconnected',
    'permission_missing',
]);

/**
 * Returns true only when the Browser Provider is present but none of its core
 * capabilities is ready and the edge reports a condition the owner can repair
 * through the existing remote desktop. No pairing secret is projected here.
 */
export function requiresBrowserRemoteTakeover(
    entries: readonly BrowserTakeoverEntry[] | null | undefined,
): boolean {
    const browserEntries = (entries ?? []).filter((entry) =>
        BROWSER_CAPABILITY_IDS.has(entry.capability.capability_id),
    );
    return browserEntries.length > 0
        && !browserEntries.some((entry) => entry.ready)
        && browserEntries.some((entry) => entry.reason && TAKEOVER_REASONS.has(entry.reason));
}
