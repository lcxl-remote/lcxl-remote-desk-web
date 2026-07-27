#!/usr/bin/env node
// Fails the build when a `DeskErrorCode` value is written as a bare number.
//
// The backend publishes every code through the OpenAPI spec, so the generated
// client exposes `deskErrorCodeEnum` with named members. A hand-written mirror
// compiles just as well and stays silently wrong when the backend value moves —
// which is the whole failure this codegen exists to prevent. The generated
// constants are the only supported source.
//
// Two shapes are rejected:
//
//   1. Comparing a `code` / `error_code` / `errorCode` against a numeric literal.
//   2. Declaring `const NAME = <number>` under a comment that names
//      `DeskErrorCode` — the "mirror the backend constant here" pattern.
//
// A line preceded by a `verify-error-codes: allow` comment is skipped, for the
// rare case where a literal genuinely is not an error code.

const fs = require("node:fs");
const path = require("node:path");

const ROOT = path.resolve(__dirname, "..", "src");
// Kubb output — this is where the generated constants come from.
const SKIP_DIRS = new Set(["services", "node_modules"]);

const ALLOW_MARKER = "verify-error-codes: allow";

// `(?<![\w$])` before the bare `code` alternative keeps `exit_code`, `keyCode`
// and friends out; `error_code` / `errorCode` are matched in full.
const NUMERIC_COMPARISON = /(?<![\w$])(?:error_code|errorCode|code)\s*(?:===|!==|==|!=)\s*-?\d+/;
const NUMERIC_CONST = /^\s*(?:export\s+)?const\s+[A-Z][A-Z0-9_]*\s*(?::\s*number\s*)?=\s*-?\d+\s*;?\s*$/;
const MENTIONS_ERROR_CODE = /DeskErrorCode/;
// How far above a constant a `DeskErrorCode` comment still counts as labelling it.
const COMMENT_LOOKBEHIND = 3;

const violations = [];

function checkFile(file) {
    const lines = fs.readFileSync(file, "utf8").split(/\r?\n/);
    lines.forEach((line, index) => {
        if (index > 0 && lines[index - 1].includes(ALLOW_MARKER)) return;

        if (NUMERIC_COMPARISON.test(line)) {
            violations.push({
                file,
                line: index + 1,
                text: line.trim(),
                why: "compares an error code against a numeric literal",
            });
            return;
        }

        if (NUMERIC_CONST.test(line)) {
            const above = lines.slice(Math.max(0, index - COMMENT_LOOKBEHIND), index);
            if (above.some((l) => MENTIONS_ERROR_CODE.test(l))) {
                violations.push({
                    file,
                    line: index + 1,
                    text: line.trim(),
                    why: "declares a hand-written mirror of a DeskErrorCode value",
                });
            }
        }
    });
}

function walk(dir) {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
        const full = path.join(dir, entry.name);
        if (entry.isDirectory()) {
            if (!SKIP_DIRS.has(entry.name)) walk(full);
        } else if (/\.(ts|tsx)$/.test(entry.name)) {
            checkFile(full);
        }
    }
}

if (!fs.existsSync(ROOT)) {
    console.error(`verify-error-codes: ${ROOT} does not exist`);
    process.exit(1);
}

walk(ROOT);

if (violations.length > 0) {
    console.error(
        "verify-error-codes: found hand-written DeskErrorCode values. Import the " +
            "generated constants instead:\n" +
            "    import { deskErrorCodeEnum } from '<services>/types'\n",
    );
    for (const v of violations) {
        console.error(`  ${path.relative(ROOT, v.file)}:${v.line} — ${v.why}`);
        console.error(`    ${v.text}`);
    }
    process.exit(1);
}

console.log("verify-error-codes: no hand-written error-code values found.");
