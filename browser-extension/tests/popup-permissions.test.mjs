import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { parsePairingSettings } from '../src/pairing-settings.js';
import { permissionPatternForUrl } from '../src/host-permissions.js';

const source = readFileSync(new URL('../src/popup.js', import.meta.url), 'utf8')
    .replace(/^import .*;\r?\n/gm, '');

function popup(request) {
    const elements = new Map();
    const document = {
        documentElement: {},
        querySelectorAll: () => [],
        querySelector(selector) {
            if (!elements.has(selector)) elements.set(selector, {
                textContent: '', value: '', handlers: {},
                addEventListener(event, callback) { this.handlers[event] = callback; },
            });
            return elements.get(selector);
        },
    };
    const chrome = {
        i18n: { getMessage: key => key, getUILanguage: () => 'en' },
        storage: {
            session: { get: async () => ({}) }, local: { get: async () => ({}) },
            onChanged: { addListener() {} },
        },
        permissions: { request },
    };
    runInNewContext(source, { chrome, document, permissionPatternForUrl, parsePairingSettings });
    return {
        click: () => elements.get('#allow-all-web').handlers.click(),
        status: () => elements.get('#status').textContent,
    };
}

test('all web access is requested only on click and excludes file URLs', async () => {
    const calls = [];
    const ui = popup(options => { calls.push(JSON.parse(JSON.stringify(options))); return Promise.resolve(true); });
    assert.equal(calls.length, 0);
    const pending = ui.click();
    assert.deepEqual(calls, [{ origins: ['https://*/*', 'http://*/*'] }]);
    await pending;
    assert.equal(ui.status(), 'allWebAllowed');
});

test('declined broad permission is not reported as granted', async () => {
    const ui = popup(async () => false);
    await ui.click();
    assert.equal(ui.status(), 'siteDenied');
});

test('Chrome permission errors are shown without an unhandled rejection', async () => {
    const ui = popup(async () => { throw new Error('not allowed'); });
    await ui.click();
    assert.equal(ui.status(), 'permissionRequestFailed');
});
