import { describe, it, expect, beforeEach } from 'vitest';

import {
    saveSessionGrant,
    readSessionGrant,
    clearSessionGrant,
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
});
