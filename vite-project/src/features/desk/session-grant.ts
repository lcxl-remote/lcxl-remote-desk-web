// Per-target access-grant store for the control end.
//
// When a code is redeemed the server returns a `grant_session_id` and the code's
// capability ceiling (`access_ceiling`). Every RequestRemote for that target — the
// main session and the separate file-transfer connection — must carry the
// grant_session_id so the trusted central can look the grant up and stamp the
// ceiling. This module holds that grant, keyed by the target connection id (the
// route `:id`), in sessionStorage so it survives the navigation from redeem to the
// desk view and a page reload within the same tab, but never leaks across tabs or
// outlives the tab.
//
// An owner connection (from the device list, full control) mints no grant; opening
// such a session clears any stale grant left for that target so a residual restricted
// token can never silently downgrade a subsequent owner session.

import type { SecuritySettings } from '@/services/types';

/// Where a stored grant came from. Both are capability-scoped (restricted); the
/// distinction drives copy ("temporary support" vs "device code") and nothing security-relevant.
export type GrantSource = 'device-code' | 'support';

export interface SessionGrant {
    /// The reusable grant token attached to every RequestRemote for this target.
    grantSessionId: string;
    /// The code's capability ceiling, used only to hide entries a dimension explicitly
    /// denies. `null` is treated as "no explicit ceiling" (show everything as tryable).
    accessCeiling: SecuritySettings | null;
    /// Origin of the grant (copy only).
    source: GrantSource;
}

const GRANT_KEY_PREFIX = 'desk-grant:';

const keyFor = (deskId: string) => `${GRANT_KEY_PREFIX}${deskId}`;

/// Persist a restricted grant for a target. Called right after a successful redeem,
/// before navigating to the desk view.
export function saveSessionGrant(deskId: string, grant: SessionGrant): void {
    try {
        sessionStorage.setItem(keyFor(deskId), JSON.stringify(grant));
    } catch {
        // Storage disabled / quota exceeded: the session degrades to the backend's
        // fail-closed enforcement (host still requires the stamped grant), so a
        // failure here only costs the UX hints, never safety.
    }
}

/// Read the restricted grant for a target, or `null` if this is an owner/full session
/// (or the grant could not be parsed).
export function readSessionGrant(deskId: string): SessionGrant | null {
    try {
        const raw = sessionStorage.getItem(keyFor(deskId));
        if (!raw) return null;
        const parsed = JSON.parse(raw) as SessionGrant;
        if (typeof parsed?.grantSessionId !== 'string' || parsed.grantSessionId.length === 0) {
            return null;
        }
        return parsed;
    } catch {
        return null;
    }
}

/// Drop any stored grant for a target. Called on the owner/full-control path so a
/// previous restricted grant for the same target cannot downgrade the owner session.
export function clearSessionGrant(deskId: string): void {
    try {
        sessionStorage.removeItem(keyFor(deskId));
    } catch {
        // Best-effort; see saveSessionGrant.
    }
}

/// Drop every stored grant, for all targets. Called on logout so one account's
/// redeemed grants never linger into the next account signed in on the same tab
/// (they would be rejected server-side by principal, but must not leak as UX state
/// either). Mirrors the native clients' `clearAllGrants` on sign-out.
export function clearAllGrants(): void {
    try {
        const keys: string[] = [];
        for (let i = 0; i < sessionStorage.length; i++) {
            const key = sessionStorage.key(i);
            if (key?.startsWith(GRANT_KEY_PREFIX)) keys.push(key);
        }
        keys.forEach((key) => sessionStorage.removeItem(key));
    } catch {
        // Best-effort; see saveSessionGrant.
    }
}
