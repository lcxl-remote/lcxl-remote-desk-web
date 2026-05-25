#!/usr/bin/env node
// Fails the build if `src/services/` ever contains a subdirectory whose
// name matches /undefined/i — that's Kubb's fallback when the OpenAPI
// `tags` field is empty, which means a controller's `#[utoipa::path]`
// annotation lost its `tag = ...`. Acts as a tripwire so a stale
// regeneration cannot land an `undefinedController/` group again.

const fs = require("node:fs");
const path = require("node:path");

const ROOT = path.resolve(__dirname, "..", "src", "services");

if (!fs.existsSync(ROOT)) {
    console.error(`verify-services: ${ROOT} does not exist`);
    process.exit(1);
}

const violations = [];
function walk(dir) {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
        if (!entry.isDirectory()) continue;
        if (/undefined/i.test(entry.name)) {
            violations.push(path.join(dir, entry.name));
        }
        walk(path.join(dir, entry.name));
    }
}

walk(ROOT);

if (violations.length > 0) {
    console.error(
        "verify-services: found `undefined*` directories under src/services — " +
            "the OpenAPI spec is missing tags on one or more operations.",
    );
    for (const v of violations) console.error("  " + v);
    process.exit(1);
}
