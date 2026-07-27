#!/usr/bin/env node
// Fails the build when a `DeskErrorCode` value is written as a bare number.
//
// The backend publishes every code through the OpenAPI spec, so the generated
// client exposes `deskErrorCodeEnum` with named members. A hand-written mirror
// compiles just as well and stays silently wrong when the backend value moves —
// which is the whole failure this codegen exists to prevent. The generated
// constants are the only supported source.
//
// Three shapes are rejected:
//
//   1. Comparing a `code` / `error_code` / `errorCode` against a numeric literal.
//   2. Declaring `const NAME = <number>` where NAME is one of the generated code
//      names — the mirror pattern, regardless of how it is commented.
//   3. Declaring `const NAME = <number>` under a comment naming `DeskErrorCode`,
//      which catches a mirror hiding behind a renamed alias.
//
// A line preceded by a `verify-error-codes: allow` comment is skipped, for the
// rare case where a literal genuinely is not an error code.

const fs = require("node:fs");
const path = require("node:path");

const SRC = path.resolve(__dirname, "..", "src");
// Kubb output — this is where the generated constants come from.
const GENERATED_TYPES = path.join(SRC, "services", "types.ts");
const SKIP_DIRS = new Set(["services", "node_modules"]);

const ALLOW_MARKER = "verify-error-codes: allow";

// `(?<![\w$])` before the bare `code` alternative keeps `exit_code`, `keyCode`
// and friends out; `error_code` / `errorCode` are matched in full.
const NUMERIC_COMPARISON = /(?<![\w$])(?:error_code|errorCode|code)\s*(?:===|!==|==|!=)\s*-?\d+/;
const NUMERIC_CONST = /^\s*(?:export\s+)?const\s+([A-Z][A-Z0-9_]*)\s*(?::\s*number\s*)?=\s*-?\d+\s*;?\s*$/;
const MENTIONS_ERROR_CODE = /DeskErrorCode/;
// How far above a constant a `DeskErrorCode` comment still counts as labelling it.
const COMMENT_LOOKBEHIND = 3;

/**
 * Names of the generated `deskErrorCodeEnum` members. Read from the generated
 * file rather than hard-coded here: a second list would be one more mirror to
 * drift, which is exactly what this check exists to stop.
 */
function generatedCodeNames() {
    if (!fs.existsSync(GENERATED_TYPES)) {
        console.error(
            `verify-error-codes: ${GENERATED_TYPES} is missing — regenerate the client first.`,
        );
        process.exit(1);
    }
    const source = fs.readFileSync(GENERATED_TYPES, "utf8");
    const block = source.match(/export const deskErrorCodeEnum = \{([\s\S]*?)\} as const;/);
    if (!block) {
        console.error(
            "verify-error-codes: the generated client has no `deskErrorCodeEnum`. The " +
                "backend must publish `DeskErrorCode` in its OpenAPI spec; regenerate the " +
                "client after checking the schema registration.",
        );
        process.exit(1);
    }
    const names = new Set();
    for (const [, name] of block[1].matchAll(/^\s*([A-Z][A-Z0-9_]*)\s*:/gm)) {
        names.add(name);
    }
    if (names.size === 0) {
        console.error("verify-error-codes: `deskErrorCodeEnum` is empty.");
        process.exit(1);
    }
    return names;
}

const CODE_NAMES = generatedCodeNames();
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

        const declaration = NUMERIC_CONST.exec(line);
        if (!declaration) return;

        if (CODE_NAMES.has(declaration[1])) {
            violations.push({
                file,
                line: index + 1,
                text: line.trim(),
                why: `mirrors the generated \`deskErrorCodeEnum.${declaration[1]}\``,
            });
            return;
        }

        const above = lines.slice(Math.max(0, index - COMMENT_LOOKBEHIND), index);
        if (above.some((l) => MENTIONS_ERROR_CODE.test(l))) {
            violations.push({
                file,
                line: index + 1,
                text: line.trim(),
                why: "declares a numeric constant under a DeskErrorCode comment",
            });
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

if (!fs.existsSync(SRC)) {
    console.error(`verify-error-codes: ${SRC} does not exist`);
    process.exit(1);
}

walk(SRC);

if (violations.length > 0) {
    console.error(
        "verify-error-codes: found hand-written DeskErrorCode values. Import the " +
            "generated constants instead:\n" +
            "    import { deskErrorCodeEnum } from '<services>/types'\n" +
            `(If a flagged literal is genuinely not an error code, put a "${ALLOW_MARKER}" ` +
            "comment on the line above.)\n",
    );
    for (const v of violations) {
        console.error(`  ${path.relative(SRC, v.file)}:${v.line} — ${v.why}`);
        console.error(`    ${v.text}`);
    }
    process.exit(1);
}

console.log(
    `verify-error-codes: no hand-written error-code values found (${CODE_NAMES.size} generated codes).`,
);
