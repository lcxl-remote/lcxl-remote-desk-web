// Restricted-session state for the desk views.
//
// Derived from the per-target grant stored at redeem time (see `session-grant.ts`).
// A session is "restricted" when a grant is present (a redeemed device / support
// code); an owner session (from the device list) has no grant and is unrestricted.
//
// This drives entry visibility only — a UX convenience, not a security boundary. The
// host independently enforces `meet(ceiling, global)` plus live approval and
// fail-closes, so a control end that ignores this gains nothing. Two visibility rules:
//   - Owner-plane entries (settings, AI diagnose, virtual display) are hidden in a
//     restricted session: they are not grantable capabilities at all.
//   - A capability entry is shown unless the ceiling explicitly denies it (`false`);
//     an unset dimension (`null`/prompt) stays visible-and-tryable, because the host
//     may still allow it after an on-device approval.
//
// The desk routes are flat siblings sharing the `:id` target, so each derives its own
// state from the route param via `useRestrictedSession()`; there is no wrapping
// provider to prop-drill through.

import { useMemo } from 'react';
import { useParams } from 'react-router-dom';

import type { SecuritySettings } from '@/services/types';
import { readSessionGrant } from './session-grant';

// The capability dimensions an entry can map to (a subset of SecuritySettings).
export type CapabilityKey =
    | 'allow_remote_control'
    | 'allow_clipboard_sync'
    | 'allow_private_screen'
    | 'allow_whiteboard'
    | 'allow_terminal'
    | 'allow_file_browse'
    | 'allow_file_transfer';

export interface RestrictedSession {
    // True when the current session was opened by redeeming a code.
    isRestricted: boolean;
    // The grant token to attach to RequestRemote, or null for an owner session.
    grantSessionId: string | null;
    // The code's ceiling, or null when unrestricted / unconfigured.
    ceiling: SecuritySettings | null;
    // Whether owner-plane entries (settings / diagnose / virtual display) should show.
    ownerPlaneVisible: boolean;
    // Whether a capability entry should show (hidden only when the ceiling denies it).
    capabilityVisible: (key: CapabilityKey) => boolean;
}

// Derive the restriction state for a target from its stored grant. Pure — exported
// for unit tests.
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

// Hook form: derive the restriction for the current route's `:id` (or an explicit
// target), memoized on the target.
export function useRestrictedSession(deskId?: string): RestrictedSession {
    const { id } = useParams<{ id: string }>();
    const target = deskId ?? id;
    return useMemo(() => deriveRestrictedSession(target), [target]);
}
