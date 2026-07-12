import { describe, it, expect, beforeEach } from 'vitest';

import {
    saveSessionGrant,
    readSessionGrant,
    clearSessionGrant,
    clearAllGrants,
} from './session-grant';
import { deriveRestrictedSession } from './restricted-session';
import type { SecuritySettings } from '@/services/types';

const allPrompt = (): SecuritySettings => ({});

describe('session-grant storage', () => {
    beforeEach(() => sessionStorage.clear());

    it('round-trips a saved grant and clears it', () => {
        saveSessionGrant('desk-1', {
            grantSessionId: 'gs-1',
            accessCeiling: { allow_terminal: true },
            source: 'device-code',
        });
        const read = readSessionGrant('desk-1');
        expect(read?.grantSessionId).toBe('gs-1');
        expect(read?.source).toBe('device-code');
        expect(read?.accessCeiling?.allow_terminal).toBe(true);

        clearSessionGrant('desk-1');
        expect(readSessionGrant('desk-1')).toBeNull();
    });

    it('is scoped per target', () => {
        saveSessionGrant('desk-a', { grantSessionId: 'gs-a', accessCeiling: null, source: 'support' });
        expect(readSessionGrant('desk-a')?.grantSessionId).toBe('gs-a');
        expect(readSessionGrant('desk-b')).toBeNull();
    });

    it('ignores a grant with an empty token', () => {
        sessionStorage.setItem('desk-grant:desk-x', JSON.stringify({ grantSessionId: '', accessCeiling: null }));
        expect(readSessionGrant('desk-x')).toBeNull();
    });

    it('clearAllGrants drops every target but leaves unrelated keys', () => {
        saveSessionGrant('desk-a', { grantSessionId: 'gs-a', accessCeiling: null, source: 'support' });
        saveSessionGrant('desk-b', { grantSessionId: 'gs-b', accessCeiling: null, source: 'device-code' });
        sessionStorage.setItem('unrelated', 'keep-me');

        clearAllGrants();

        expect(readSessionGrant('desk-a')).toBeNull();
        expect(readSessionGrant('desk-b')).toBeNull();
        expect(sessionStorage.getItem('unrelated')).toBe('keep-me');
    });
});

describe('deriveRestrictedSession', () => {
    beforeEach(() => sessionStorage.clear());

    it('is unrestricted with no grant (owner session): everything visible', () => {
        const r = deriveRestrictedSession('desk-1');
        expect(r.isRestricted).toBe(false);
        expect(r.grantSessionId).toBeNull();
        expect(r.ownerPlaneVisible).toBe(true);
        expect(r.capabilityVisible('allow_terminal')).toBe(true);
        expect(r.capabilityVisible('allow_file_browse')).toBe(true);
    });

    it('hides only the dimensions the ceiling explicitly denies', () => {
        saveSessionGrant('desk-1', {
            grantSessionId: 'gs-1',
            accessCeiling: { allow_terminal: false, allow_clipboard_sync: true, ...allPrompt() },
            source: 'device-code',
        });
        const r = deriveRestrictedSession('desk-1');
        expect(r.isRestricted).toBe(true);
        expect(r.grantSessionId).toBe('gs-1');
        // Owner-plane entries are always hidden in a restricted session.
        expect(r.ownerPlaneVisible).toBe(false);
        // Explicit deny (false) hides; allow (true) and unset (prompt) stay visible.
        expect(r.capabilityVisible('allow_terminal')).toBe(false);
        expect(r.capabilityVisible('allow_clipboard_sync')).toBe(true);
        expect(r.capabilityVisible('allow_file_browse')).toBe(true); // unset => tryable
    });

    it('treats a null ceiling as all-tryable but still restricted', () => {
        saveSessionGrant('desk-1', { grantSessionId: 'gs-1', accessCeiling: null, source: 'support' });
        const r = deriveRestrictedSession('desk-1');
        expect(r.isRestricted).toBe(true);
        expect(r.ownerPlaneVisible).toBe(false);
        expect(r.capabilityVisible('allow_terminal')).toBe(true);
    });

    it('anti-downgrade: clearing the grant before an owner connect restores full control', () => {
        // Redeem a restricted code for a target in this tab...
        saveSessionGrant('desk-1', {
            grantSessionId: 'gs-1',
            accessCeiling: { allow_terminal: false, allow_file_browse: false },
            source: 'device-code',
        });
        expect(deriveRestrictedSession('desk-1').isRestricted).toBe(true);

        // ...then connect to the SAME target from the owner device list, which clears
        // the stale grant first. The owner session must derive as unrestricted, so no
        // grant token is sent and the residual code cannot downgrade it.
        clearSessionGrant('desk-1');
        const owner = deriveRestrictedSession('desk-1');
        expect(owner.isRestricted).toBe(false);
        expect(owner.grantSessionId).toBeNull();
        expect(owner.ownerPlaneVisible).toBe(true);
        expect(owner.capabilityVisible('allow_terminal')).toBe(true);
        expect(owner.capabilityVisible('allow_file_browse')).toBe(true);
    });
});
