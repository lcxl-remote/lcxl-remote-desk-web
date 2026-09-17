import { test } from 'node:test';
import assert from 'node:assert/strict';
import { webcrypto, createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import { registerContentDigest } from '../src/content-digest.js';

const source = readFileSync(new URL('../src/content-script.js', import.meta.url), 'utf8');

function digestBridge(permission = true) {
    let listener;
    const api = {
        runtime: { id: 'extension', onMessage: { addListener(fn) { listener = fn; } } },
        permissions: { contains: async () => permission },
    };
    registerContentDigest(api);
    return (message, sender = { id: 'extension', tab: { id: 1 }, url: 'http://192.168.1.20:8080/' }) =>
        new Promise(resolve => listener(message, sender, resolve));
}

test('HTTP content script describes a page without secure-context crypto APIs', async () => {
    let listener;
    const href = 'http://192.168.1.20:8080/app';
    const sendMessage = digestBridge();
    vm.runInNewContext(source, {
        crypto: { getRandomValues: array => webcrypto.getRandomValues(array) },
        URL, TextEncoder, TextDecoder, btoa, Uint8Array,
        location: new URL(href),
        document: { title: 'LAN application' },
        MutationObserver: class { observe() {} },
        chrome: { runtime: { sendMessage, onMessage: { addListener(fn) { listener = fn; } } } },
    });
    const reply = await new Promise(resolve => listener({ type: 'lcxl_browser_action', action: { action: 'describe_page' } }, {}, resolve));
    assert.equal(reply.ok, true);
    assert.equal(reply.result.page.origin.kind, 'http');
    assert.equal(reply.result.page.origin.port, 8080);
    assert.equal(reply.result.page.url_sha256, createHash('sha256').update(href).digest('hex'));
});

test('digest bridge rejects unprivileged senders and revoked site access', async () => {
    const message = { type: 'lcxl_content_digest', base64: btoa('example') };
    assert.equal((await digestBridge()(message, { id: 'other', tab: { id: 1 }, url: 'http://example.com/' })).error, 'digest_unavailable');
    assert.equal((await digestBridge(false)(message)).error, 'digest_unavailable');
});
