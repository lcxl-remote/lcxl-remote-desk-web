// Restricted-session state for the desk views.
//
// Derived from the per-target grant stored at redeem time (see `session-grant.ts`).
// A session is "restricted" when a grant is present (a redeemed device / support
// code); an owner session (from the device list) has no grant and is unrestricted.
//
// This drives entry visibility only — a UX convenience, not a security boundary. The
// host independently enforces `meet(ceiling, global)` plus live approval and
// fail-closes, so a control end that ignores this gains nothing. Two visibility rules:
//   - Owner-plane entries (settings, AI Assistant, virtual display) are hidden in a
//     restricted session: they are not grantable capabilities at all.
//   - A capability entry is shown unless the ceiling explicitly denies it (`false`);
//     an unset dimension (`null`/prompt) stays visible-and-tryable, because the host
//     may still allow it after an on-device approval.
//
// The target boundary separates stable owner routes from fixed grant sessions.

import { useMemo } from 'react';
import { useOptionalDeviceTarget } from './device-target-context';

import type { SecuritySettings } from '@/services/types';
import { readSessionGrant } from './session-grant';

// The capability dimensions an entry can map to (a subset of SecuritySettings).
export type CapabilityKey =
    | 'allow_remote_control'
    | 'allow_clipboard_sync'
    | 'allow_system_audio_capture'
    | 'allow_private_screen'
    | 'allow_whiteboard'
    | 'allow_terminal'
    | 'allow_file_browse'
    | 'allow_file_delete'
    | 'allow_file_transfer';

export interface RestrictedSession {
    // True when the current session was opened by redeeming a code.
    isRestricted: boolean;
    // The grant token to attach to RequestRemoteAccess, or null for an owner session.
    grantSessionId: string | null;
    // The code's ceiling, or null when unrestricted / unconfigured.
    ceiling: SecuritySettings | null;
    // Whether owner-plane entries (settings / AI Assistant / virtual display) should show.
    ownerPlaneVisible: boolean;
    // Whether a capability entry should show (hidden only when the ceiling denies it).
    capabilityVisible: (key: CapabilityKey) => boolean;
}

// Derive the restriction state for a target from its stored grant. Pure — exported
// for unit tests.
//
// Stable device routes use authorized discovery. Fixed finite routes require a
// matching principal-bound grant before mounting and always hide owner entries.
export function deriveRestrictedSession(deskId: string | undefined): RestrictedSession {
    const grant = deskId ? readSessionGrant(deskId) : null;
    const isRestricted = grant != null;
    const ceiling = grant?.accessCeiling ?? null;
    return {
        isRestricted,
        grantSessionId: grant?.grantSessionId ?? null,
        ceiling,
        ownerPlaneVisible: !isRestricted,
        capabilityVisible: (key: CapabilityKey) => {
            if (!isRestricted) return true;
            // Only an explicit `false` (deny) hides the entry; `null`/prompt and
            // `true`/allow both stay visible-and-tryable.
            return ceiling?.[key] !== false;
        },
    };
}

export function useRestrictedSession(deskId?: string): RestrictedSession {
    const target = useOptionalDeviceTarget();
    const id = deskId ?? target?.connectionId;
    const finite = target?.kind === 'session';
    return useMemo(() => {
        const restricted = deriveRestrictedSession(target?.kind === 'device' ? undefined : id);
        return finite ? { ...restricted, isRestricted: true, ownerPlaneVisible: false } : restricted;
    }, [id, finite, target?.kind]);
}
