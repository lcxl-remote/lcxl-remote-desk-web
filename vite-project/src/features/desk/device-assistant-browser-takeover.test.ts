import { describe, expect, it } from 'vitest';

import { requiresBrowserRemoteTakeover } from './device-assistant-browser-takeover';

const entry = (capabilityId: string, ready: boolean, reason?: string) => ({
    capability: { capability_id: capabilityId },
    ready,
    reason,
});

describe('requiresBrowserRemoteTakeover', () => {
    it('offers takeover for a disconnected or unpaired Browser Provider', () => {
        expect(requiresBrowserRemoteTakeover([
            entry('browser.page.snapshot', false, 'adapter_unavailable'),
            entry('browser.page.open', false, 'permission_missing'),
        ])).toBe(true);
    });

    it('does not offer takeover when any core browser capability is ready', () => {
        expect(requiresBrowserRemoteTakeover([
            entry('browser.page.snapshot', true),
            entry('browser.page.open', false, 'permission_missing'),
        ])).toBe(false);
    });

    it('does not treat policy or unrelated providers as a pairing problem', () => {
        expect(requiresBrowserRemoteTakeover([
            entry('browser.page.snapshot', false, 'local_ceiling'),
            entry('desktop.session.inspect', false, 'adapter_unavailable'),
        ])).toBe(false);
        expect(requiresBrowserRemoteTakeover(undefined)).toBe(false);
    });
});
