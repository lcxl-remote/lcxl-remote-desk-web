// Test-only JSON-lines peer for the Rust transport interoperability test.
import { createInterface } from "node:readline";
import { createBridgeCipher } from "../src/bridge-crypto.js";

let cipher;
for await (const line of createInterface({ input: process.stdin })) {
    try {
        const request = JSON.parse(line);
        let result;
        switch (request.op) {
            case "hello":
                cipher = await createBridgeCipher(request.secret, request.metadata);
                result = cipher.hello;
                break;
            case "challenge": result = await cipher.challenge(request.text); break;
            case "open": result = await cipher.open(request.text); break;
            case "seal": result = await cipher.seal(request.text); break;
            default: throw new Error("invalid test operation");
        }
        process.stdout.write(JSON.stringify({ ok: true, result }) + "\n");
    } catch (error) {
        process.stdout.write(JSON.stringify({ ok: false, error: error.message }) + "\n");
    }
}
