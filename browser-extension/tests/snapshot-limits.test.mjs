import assert from 'node:assert/strict';
import { webcrypto } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import vm from 'node:vm';

const source = await readFile(new URL('../src/content-script.js', import.meta.url), 'utf8');

async function capture(name, value) {
    class Input {
        tagName = 'INPUT';
        type = 'text';
        isConnected = true;
        constructor() { this.value = value; }
        getAttribute(attribute) { return attribute === 'aria-label' ? name : null; }
        getBoundingClientRect() { return { width: 100, height: 20 }; }
    }
    let listener;
    const context = vm.createContext({
        crypto: webcrypto, TextEncoder, TextDecoder, URL,
        HTMLInputElement: Input, HTMLTextAreaElement: class {}, HTMLSelectElement: class {},
        location: new URL('https://example.test/'),
        document: { querySelectorAll: () => [new Input()] },
        getComputedStyle: () => ({ visibility: 'visible', display: 'block' }),
        chrome: { runtime: { onMessage: { addListener: fn => { listener = fn; } } } },
    });
    vm.runInContext(source, context);
    const response = await new Promise(resolve => listener({
        type: 'lcxl_browser_action', action: { action: 'take_snapshot', max_elements: 64 },
    }, {}, resolve));
    assert.equal(response.ok, true);
    return response.result.snapshot;
}

test('field clipping marks an otherwise complete single-element snapshot', async () => {
    const exact = await capture('n'.repeat(1024), 'v'.repeat(65536));
    assert.equal(exact.truncated, false);
    for (const [name, value] of [['中'.repeat(400), 'value'], ['name', '中'.repeat(22000)]]) {
        const snapshot = await capture(name, value);
        assert.equal(snapshot.elements.length, 1);
        assert.equal(snapshot.truncated, true);
        assert.ok(new TextEncoder().encode(snapshot.elements[0].accessible_name).length <= 1024);
        assert.ok(new TextEncoder().encode(snapshot.elements[0].value).length <= 65536);
        assert.ok(!snapshot.elements[0].accessible_name.includes('\ufffd'));
        assert.ok(!snapshot.elements[0].value.includes('\ufffd'));
    }
});
