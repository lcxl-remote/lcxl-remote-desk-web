import assert from 'node:assert/strict';
import test from 'node:test';
import { runBoundedCommand } from '../src/command-lifetime.js';

test('expired work cannot submit when an unresolved read eventually returns', async () => {
    let resolveRead;
    const read = new Promise(resolve => { resolveRead = resolve; });
    let continuation;
    let writes = 0;
    let closed = 0;
    const result = runBoundedCommand(guard => {
        continuation = (async () => {
            await read;
            guard();
            writes++;
        })();
        return continuation;
    }, () => true, () => { closed++; }, 10);
    await assert.rejects(result, /bridge_command_timeout/);
    assert.equal(closed, 1);
    resolveRead();
    await assert.rejects(continuation, /bridge_command_expired/);
    assert.equal(writes, 0);
});

test('completed work cannot use its old guard again', async () => {
    let saved;
    const result = await runBoundedCommand(guard => { saved = guard; guard(); return 'done'; }, () => true, () => {});
    assert.equal(result, 'done');
    assert.throws(() => saved(), /bridge_command_expired/);
});

test('a replaced connection prevents initial dispatch', async () => {
    let ran = false;
    await assert.rejects(runBoundedCommand(() => { ran = true; }, () => false, () => {}), /bridge_command_expired/);
    assert.equal(ran, false);
});
