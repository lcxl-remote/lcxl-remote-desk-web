import test from 'node:test';
import assert from 'node:assert/strict';
import { parsePairingSettings } from '../src/pairing-settings.js';
const pairingToken = 'test-pairing-secret-1234';
const bridgeUrl = 'ws://127.0.0.1:12345/browser-extension/v2';
test('accepts an explicit canonical paired loopback endpoint', () => {
    assert.deepEqual(parsePairingSettings({ bridgeUrl, pairingToken }), { bridgeUrl, pairingToken });
});
test('rejects remote endpoints, aliases, credentials and obsolete wire paths', () => {
    for (const endpoint of [
        'ws://localhost:12345/browser-extension/v2',
        'ws://127.1:12345/browser-extension/v2',
        'ws://2130706433:12345/browser-extension/v2',
        'ws://example.com/browser-extension/v2',
        'ws://user@127.0.0.1:12345/browser-extension/v2',
        'ws://127.0.0.1:0/browser-extension/v2',
        bridgeUrl + '?token=secret', bridgeUrl + '#fragment',
        bridgeUrl.replace('/v2', '/v1'), '',
    ]) assert.equal(parsePairingSettings({ bridgeUrl: endpoint, pairingToken }), null);
});
test('rejects missing, short, non-ASCII and oversized secrets', () => {
    for (const token of [undefined, '', 'short', 'a'.repeat(257), '密'.repeat(20), 'a'.repeat(16) + '\ninner']) {
        assert.equal(parsePairingSettings({ bridgeUrl, pairingToken: token }), null);
    }
});
