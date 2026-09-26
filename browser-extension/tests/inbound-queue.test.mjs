import test from 'node:test';
import assert from 'node:assert/strict';
import { createInboundQueue } from '../src/inbound-queue.js';
const flush = () => new Promise(resolve => setImmediate(resolve));

test('overflow drops queued messages and closes even with an unresolved consumer', async () => {
    let release;
    const hold = new Promise(resolve => { release = resolve; });
    const seen = [];
    let closed = 0;
    const queue = createInboundQueue(async text => { seen.push(text); await hold; }, () => { closed++; }, { maxItems: 2 });
    assert.equal(queue.push('first'), true);
    assert.equal(queue.push('second'), true);
    assert.equal(queue.push('overflow'), false);
    assert.equal(closed, 1);
    release();
    await flush();
    assert.deepEqual(seen, ['first']);
    assert.equal(queue.push('later'), false);
});

test('total retained character budget includes the running message', () => {
    let closed = false;
    const queue = createInboundQueue(() => new Promise(() => {}), () => { closed = true; }, { maxCharacters: 5 });
    assert.equal(queue.push('1234'), true);
    assert.equal(queue.push('56'), false);
    assert.equal(closed, true);
});

test('queue residence consumes the deadline instead of starting a fresh timeout', async () => {
    let now = 0;
    let release;
    const hold = new Promise(resolve => { release = resolve; });
    const seen = [];
    let closed = 0;
    const queue = createInboundQueue(async text => { seen.push(text); await hold; }, () => { closed++; }, { lifetimeMs: 10, now: () => now });
    queue.push('first');
    queue.push('expired');
    now = 11;
    release();
    await flush();
    assert.deepEqual(seen, ['first']);
    assert.equal(closed, 1);
});
