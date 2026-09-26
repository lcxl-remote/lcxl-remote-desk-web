import assert from "node:assert/strict";
import test from "node:test";
import { createSendReceiptCoordinator } from "../src/send-receipts.js";

function deferred() {
    let resolve;
    const promise = new Promise(done => { resolve = done; });
    return { promise, resolve };
}
function action(key, snapshot = "snapshot") {
    return { activation_class: { idempotency_key: key, snapshot_id: snapshot, payload_sha256: "digest" } };
}
function result(value) {
    return { send_receipt: {
        idempotency_key: value.activation_class.idempotency_key,
        snapshot_id: value.activation_class.snapshot_id,
        snapshot_sha256: value.activation_class.payload_sha256,
        observed_at_unix_ms: 42
    } };
}

test("inflight reuse rejects a conflicting snapshot without sharing its receipt or dispatching", async () => {
    const started = deferred();
    const finish = deferred();
    let calls = 0;
    let stored = {};
    const execute = createSendReceiptCoordinator({
        read: async () => structuredClone(stored),
        write: async value => { stored = structuredClone(value); },
        pageFromAction: () => ({}),
        execute: async value => { calls++; started.resolve(); await finish.promise; return result(value); }
    });
    const first = execute(action("key"), () => {});
    await started.promise;
    await assert.rejects(execute(action("key", "different"), () => {}), /conflicting_inflight_send/u);
    const duplicate = execute(action("key"), () => {});
    finish.resolve();
    assert.deepEqual(await duplicate, await first);
    assert.equal(calls, 1);
});

test("concurrent sends cannot overwrite each other's persisted receipts", async () => {
    const firstWrite = deferred();
    const releaseWrite = deferred();
    let writes = 0;
    let stored = {};
    const execute = createSendReceiptCoordinator({
        read: async () => structuredClone(stored),
        write: async value => {
            writes++;
            if (writes === 1) { firstWrite.resolve(); await releaseWrite.promise; }
            stored = structuredClone(value);
        },
        execute: async value => result(value),
        pageFromAction: () => ({})
    });
    const first = execute(action("first"), () => {});
    const second = execute(action("second"), () => {});
    await firstWrite.promise;
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(writes, 1);
    releaseWrite.resolve();
    await Promise.all([first, second]);
    assert.deepEqual(Object.keys(stored).sort(), ["first", "second"]);
});

test("a returned receipt must match snapshot identity before persistence", async () => {
    let writes = 0;
    const execute = createSendReceiptCoordinator({
        read: async () => ({}),
        write: async () => { writes++; },
        execute: async () => result(action("key", "wrong-snapshot")),
        pageFromAction: () => ({})
    });
    await assert.rejects(execute(action("key"), () => {}), /invalid_send_receipt/u);
    assert.equal(writes, 0);
});

test("a delayed storage lookup cannot dispatch again after the original send completes", async () => {
    const started = deferred();
    const finish = deferred();
    const staleRead = deferred();
    const releaseRead = deferred();
    const persisted = deferred();
    let stored = {};
    let reads = 0;
    let calls = 0;
    const execute = createSendReceiptCoordinator({
        read: async () => {
            const snapshot = structuredClone(stored);
            if (++reads === 2) {
                staleRead.resolve();
                await releaseRead.promise;
            }
            return snapshot;
        },
        write: async value => { stored = structuredClone(value); persisted.resolve(); },
        execute: async value => {
            calls++;
            started.resolve();
            await finish.promise;
            return result(value);
        },
        pageFromAction: () => ({})
    });
    const first = execute(action("key"), () => {});
    await started.promise;
    const duplicate = execute(action("key"), () => {});
    await staleRead.promise;
    finish.resolve();
    await persisted.promise;
    releaseRead.resolve();
    await Promise.all([first, duplicate]);
    assert.equal(calls, 1);
});

test("a restarted coordinator reuses a persisted exact receipt without sending", async () => {
    let stored = {};
    let calls = 0;
    const dependencies = {
        read: async () => structuredClone(stored),
        write: async value => { stored = structuredClone(value); },
        execute: async value => { calls++; return result(value); },
        pageFromAction: () => ({ page_id: "current-page" })
    };
    const original = await createSendReceiptCoordinator(dependencies)(action("key"), () => {});
    const restarted = createSendReceiptCoordinator(dependencies);
    const replay = await restarted(action("key"), () => {});
    assert.deepEqual(replay.send_receipt, original.send_receipt);
    await assert.rejects(restarted(action("key", "other"), () => {}), /invalid_cached_send_receipt/u);
    assert.equal(calls, 1);
});

test("receipt storage failure propagates without automatically repeating the send", async () => {
    let calls = 0;
    let writes = 0;
    const execute = createSendReceiptCoordinator({
        read: async () => ({}),
        write: async () => { writes++; throw new Error("storage-unavailable"); },
        execute: async value => { calls++; return result(value); },
        pageFromAction: () => ({})
    });
    await assert.rejects(execute(action("key"), () => {}), /storage-unavailable/u);
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(calls, 1);
    assert.equal(writes, 1);
});
