// Length-prefixed UTF-8 transcript; WebCrypto is the sole crypto implementation.
const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });
const VERSION = 2;
const MAX_SEQUENCE = 0xffffffff;
const encode = (bytes) => {
    let binary = "";
    for (let offset = 0; offset < bytes.length; offset += 8192) {
        binary += String.fromCharCode(...bytes.subarray(offset, offset + 8192));
    }
    return btoa(binary);
};
function decode(text, maximum) {
    if (typeof text !== "string" || text.length > Math.ceil(maximum / 3) * 4) throw new Error("invalid_base64");
    const bytes = Uint8Array.from(atob(text), (c) => c.charCodeAt(0));
    if (bytes.length > maximum || encode(bytes) !== text) throw new Error("invalid_base64");
    return bytes;
}
function concatenate(parts) {
    const bytes = new Uint8Array(parts.reduce((sum, part) => sum + part.length, 0));
    let offset = 0;
    for (const part of parts) { bytes.set(part, offset); offset += part.length; }
    return bytes;
}
function u32(value) {
    const bytes = new Uint8Array(4);
    new DataView(bytes.buffer).setUint32(0, value, false);
    return bytes;
}
function transcript(fields) {
    return concatenate(fields.map((field) => {
        if (typeof field !== "string") throw new Error("invalid_handshake_field");
        const bytes = encoder.encode(field);
        if (!bytes.length || bytes.length > 512) throw new Error("invalid_handshake_field");
        return concatenate([u32(bytes.length), bytes]);
    }));
}

export async function createBridgeCipher(secret, metadata) {
    const clientNonce = encode(crypto.getRandomValues(new Uint8Array(32)));
    const hello = { version: VERSION, client_nonce: clientNonce,
        extension_version: metadata.extension_version, browser_version: metadata.browser_version,
        profile_incarnation: metadata.profile_incarnation };
    const material = await crypto.subtle.importKey("raw", encoder.encode(secret), "HKDF", false, ["deriveBits"]);
    let incoming, outgoing, hash;
    let sent = 0, received = 0;
    let authenticated = false;
    let sendTail = Promise.resolve();
    let receiveTail = Promise.resolve();
    let challengeStarted = false;
    return {
        hello: JSON.stringify(hello),
        async challenge(text) {
            if (challengeStarted || typeof text !== "string" || text.length > 4096) throw new Error("invalid_challenge");
            challengeStarted = true;
            const challenge = JSON.parse(text);
            const nonce = decode(challenge.server_nonce, 32);
            if (challenge.version !== VERSION || nonce.length !== 32 || encode(nonce) !== challenge.server_nonce) throw new Error("invalid_challenge");
            const bytes = transcript(["lcxl-browser-v2", clientNonce, challenge.server_nonce, challenge.device_id, challenge.os_session_id, metadata.extension_version, metadata.browser_version, metadata.profile_incarnation]);
            hash = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
            const key = async (purpose) => crypto.subtle.deriveBits({ name: "HKDF", hash: "SHA-256", salt: hash, info: encoder.encode(`lcxl-browser-v2/${purpose}`) }, material, 256);
            const proofKey = async (purpose, usages) => crypto.subtle.importKey("raw", await key(purpose), { name: "HMAC", hash: "SHA-256" }, false, usages);
            const valid = await crypto.subtle.verify("HMAC", await proofKey("server-proof", ["verify"]), decode(challenge.server_proof, 32), bytes);
            if (!valid) throw new Error("server_authentication_failed");
            const proof = await crypto.subtle.sign("HMAC", await proofKey("client-proof", ["sign"]), bytes);
            incoming = await crypto.subtle.importKey("raw", await key("server-to-client"), "AES-GCM", false, ["decrypt"]);
            outgoing = await crypto.subtle.importKey("raw", await key("client-to-server"), "AES-GCM", false, ["encrypt"]);
            authenticated = true;
            return JSON.stringify({ version: VERSION, client_proof: encode(new Uint8Array(proof)) });
        },
        seal(plaintext) {
            // Keep nonce allocation and completion ordered even when keepalive
            // races with the result of a browser operation.
            const result = sendTail.then(async () => {
                if (!authenticated || sent >= MAX_SEQUENCE) throw new Error("cipher_unavailable");
                const payload = encoder.encode(plaintext);
                if (payload.length > 8 * 1024 * 1024 - 16) throw new Error("frame_too_large");
                const sequence = sent++;
                const iv = concatenate([new Uint8Array(8), u32(sequence)]);
                const additionalData = concatenate([hash, new Uint8Array([1]), u32(sequence)]);
                const ciphertext = await crypto.subtle.encrypt({ name: "AES-GCM", iv, additionalData, tagLength: 128 }, outgoing, payload);
                return JSON.stringify({ version: VERSION, sequence, ciphertext: encode(new Uint8Array(ciphertext)) });
            });
            sendTail = result;
            return result;
        },
        open(text) {
            const result = receiveTail.then(async () => {
            if (!authenticated || received >= MAX_SEQUENCE || typeof text !== "string" || text.length > 130 * 1024 * 1024) throw new Error("cipher_unavailable");
            const frame = JSON.parse(text);
            if (frame.version !== VERSION || !Number.isInteger(frame.sequence) || frame.sequence !== received) throw new Error("replayed_or_unordered_frame");
            const iv = concatenate([new Uint8Array(8), u32(received)]);
            const additionalData = concatenate([hash, new Uint8Array([0]), u32(received)]);
            const payload = await crypto.subtle.decrypt({ name: "AES-GCM", iv, additionalData, tagLength: 128 }, incoming, decode(frame.ciphertext, 96 * 1024 * 1024 + 16));
            received++;
            return decoder.decode(payload);
            });
            receiveTail = result;
            return result;
        }
    };
}
