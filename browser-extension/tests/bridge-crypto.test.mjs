import assert from "node:assert/strict";
import test from "node:test";
import { createBridgeCipher } from "../src/bridge-crypto.js";

const metadata = { extension_version: "1", browser_version: "test", profile_incarnation: "test-profile" };

test("metadata cannot replace protocol version or random nonce", async () => {
    const cipher = await createBridgeCipher("test-only-secret", { ...metadata, version: 0, client_nonce: "injected" });
    const hello = JSON.parse(cipher.hello);
    assert.equal(hello.version, 2);
    assert.equal(Buffer.from(hello.client_nonce, "base64").length, 32);
    assert.notEqual(hello.client_nonce, "injected");
    assert.ok(!cipher.hello.includes("test-only-secret"));
});

test("failed challenge cannot be retried or followed by plaintext traffic", async () => {
    const cipher = await createBridgeCipher("test-only-secret", metadata);
    const challenge = JSON.stringify({ version: 2, server_nonce: Buffer.alloc(32).toString("base64"),
        server_proof: Buffer.alloc(32).toString("base64"), device_id: "device", os_session_id: "session" });
    await assert.rejects(cipher.challenge(challenge), /server_authentication_failed/);
    await assert.rejects(cipher.challenge(challenge), /invalid_challenge/);
    await assert.rejects(cipher.open("{}"), /cipher_unavailable/);
    await assert.rejects(cipher.seal("private message"), /cipher_unavailable/);
});

test("noncanonical nonce is rejected", async () => {
    const cipher = await createBridgeCipher("test-only-secret", metadata);
    await assert.rejects(cipher.challenge(JSON.stringify({ version: 2,
        server_nonce: Buffer.alloc(32).toString("base64") + "\n" })), /invalid_base64/);
});
