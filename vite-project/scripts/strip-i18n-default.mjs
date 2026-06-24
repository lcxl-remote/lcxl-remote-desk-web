#!/usr/bin/env node
/**
 * Codemod: strip the inline string defaultValue from t() calls.
 *
 *   t('key', 'Default')        -> t('key')
 *   t('key', 'Default', opts)  -> t('key', opts)
 *
 * Only the second argument is removed, and only when it is a string literal on a
 * bare `t(...)` call (the identifier destructured from useTranslation). Dynamic
 * keys, member calls (i18n.t), and calls whose 2nd arg is already an options
 * object are left untouched. Formatting is preserved (no prettier).
 *
 * Usage:
 *   node scripts/strip-i18n-default.mjs --dry-run   # report only
 *   node scripts/strip-i18n-default.mjs             # apply in place
 *   node scripts/strip-i18n-default.mjs --self-test # run fixtures, no FS scan
 */
import ts from 'typescript';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const SRC = path.resolve(__dirname, '../src');

/**
 * Remove the string-literal 2nd argument from every bare t(...) call.
 * Returns { code, count }. Edits are applied right-to-left so offsets stay valid.
 */
export function transform(code, fileName = 'input.tsx') {
    const sf = ts.createSourceFile(fileName, code, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
    const edits = []; // { start, end } char range to delete (the ", 'default'")
    const visit = (node) => {
        if (ts.isCallExpression(node) && ts.isIdentifier(node.expression) && node.expression.text === 't') {
            const [a0, a1] = node.arguments;
            // a1 must be a plain string literal — never a template literal, which
            // could carry interpolation and must not be silently dropped.
            if (a0 && ts.isStringLiteralLike(a0) && a1 && ts.isStringLiteral(a1)) {
                // Delete from end of key literal up to end of the default literal,
                // i.e. the ", <whitespace> 'default'" slice; a trailing , opts stays.
                edits.push({ start: a0.getEnd(), end: a1.getEnd() });
            }
        }
        ts.forEachChild(node, visit);
    };
    visit(sf);
    edits.sort((x, y) => y.start - x.start);
    let out = code;
    for (const e of edits) out = out.slice(0, e.start) + out.slice(e.end);
    return { code: out, count: edits.length };
}

function selfTest() {
    const cases = [
        ["t('a.b', 'Hello')", "t('a.b')"],
        ["t('a.b', 'Hello', { count })", "t('a.b', { count })"],
        ["const x = t(\n    'a.b',\n    'Multi line default',\n);", "const x = t(\n    'a.b',\n);"],
        ["t('a.b', `tpl`)", "t('a.b', `tpl`)"], // template literal: not a plain string literal -> left as-is
        ["t('a.b')", "t('a.b')"], // already bare
        ["t(dynKey, 'Default')", "t(dynKey, 'Default')"], // dynamic key: untouched
        ["i18n.t('a.b', 'Default')", "i18n.t('a.b', 'Default')"], // member call: untouched
        ["t('a.b', 'X'); t('c.d', 'Y', opts);", "t('a.b'); t('c.d', opts);"],
    ];
    let ok = 0;
    for (const [input, expected] of cases) {
        const { code } = transform(input);
        if (code === expected) ok++;
        else { console.error(`FAIL\n  in : ${JSON.stringify(input)}\n  got: ${JSON.stringify(code)}\n  exp: ${JSON.stringify(expected)}`); }
    }
    console.log(`self-test: ${ok}/${cases.length} passed`);
    process.exit(ok === cases.length ? 0 : 1);
}

function walk(dir, acc = []) {
    for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
        const p = path.join(dir, e.name);
        if (e.isDirectory()) {
            if (/[\\/]services$/.test(p)) continue;
            if (p === path.join(SRC, 'locales')) continue;
            walk(p, acc);
        } else if (/\.(ts|tsx)$/.test(e.name) && !/\.test\.(ts|tsx)$/.test(e.name)) {
            acc.push(p);
        }
    }
    return acc;
}

function main() {
    if (process.argv.includes('--self-test')) return selfTest();
    const dry = process.argv.includes('--dry-run');
    let files = 0, total = 0;
    for (const file of walk(SRC)) {
        const code = fs.readFileSync(file, 'utf8');
        const { code: out, count } = transform(code, file);
        if (count > 0) {
            files++; total += count;
            if (dry) console.log(`  ${path.relative(SRC, file)}: ${count}`);
            else fs.writeFileSync(file, out);
        }
    }
    console.log(`${dry ? '[dry-run] would strip' : 'stripped'} ${total} inline default(s) across ${files} file(s)`);
}

main();
