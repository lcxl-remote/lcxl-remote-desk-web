import { describe, it, expect } from 'vitest';
import { startupModeEnum } from '@/services/types';
import { buildNavItems, startupModeLabel } from './sidebar-nav';

const urls = (ctx: Parameters<typeof buildNavItems>[0]) =>
    buildNavItems(ctx).map((item) => item.url);

describe('buildNavItems', () => {
    it('offers desk, support, usage and settings on a portable server', () => {
        expect(urls({ access: 'admin', startupMode: startupModeEnum.default })).toEqual([
            '/desk/list',
            '/support',
            '/usage',
            '/system',
        ]);
    });

    it('hides the host-only support entry on a pure signaling server', () => {
        expect(urls({ access: 'admin', startupMode: startupModeEnum.signaling })).toEqual([
            '/desk/list',
            '/usage',
            '/system',
        ]);
    });

    // The usage views read the local signal DB, which a desk-server never
    // opens; the backend does not register their endpoints there, so offering
    // the entry would only lead to a 404.
    it('hides desk and usage on a pure desk-server', () => {
        expect(urls({ access: 'admin', startupMode: startupModeEnum['desk-server'] })).toEqual([
            '/support',
            '/system',
        ]);
    });

    // The daemon has the signal DB (it opens one during bootstrap), so its
    // console keeps the usage views even though TURN never runs there.
    it('keeps usage on the service daemon', () => {
        expect(urls({ access: 'admin', startupMode: startupModeEnum['service-daemon'] })).toContain(
            '/usage',
        );
    });

    // The spellings the backend actually emits. A snake_case literal used to
    // match nothing here, so every desk-server showed the full menu; the field
    // is typed now, but the wire values are still what the gating turns on.
    it('turns on the kebab-case mode names the backend emits', () => {
        expect(startupModeEnum['desk-server']).toBe('desk-server');
        expect(startupModeEnum['service-daemon']).toBe('service-daemon');
    });

    it('scopes a redeemed device code to its one device', () => {
        expect(
            urls({
                access: 'device_user',
                targetConnectionId: 'conn-1',
                startupMode: startupModeEnum.default,
            }),
        ).toEqual(['/desk/conn-1/control']);
    });

    it('offers nothing to a device code with no target', () => {
        expect(urls({ access: 'device_user', startupMode: startupModeEnum.default })).toEqual([]);
    });
});

describe('startupModeLabel', () => {
    it('labels every mode that serves a console', () => {
        expect(startupModeLabel(startupModeEnum.default)).toBe('Default');
        expect(startupModeLabel(startupModeEnum.signaling)).toBe('Signaling');
        expect(startupModeLabel(startupModeEnum['desk-server'])).toBe('Desk Server');
        expect(startupModeLabel(startupModeEnum['service-daemon'])).toBe('Service Daemon');
    });

    // `session-worker` and `mcp-stdio` never serve a console, so they have no
    // badge text to show.
    it('falls back to an empty badge for a mode with no console', () => {
        expect(startupModeLabel(undefined)).toBe('');
        expect(startupModeLabel(startupModeEnum['session-worker'])).toBe('');
        expect(startupModeLabel(startupModeEnum['mcp-stdio'])).toBe('');
    });
});
