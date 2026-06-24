#!/usr/bin/env node
/**
 * i18n integrity gate for the web frontend.
 *
 * Enforces locale as the single source of truth after inline t() defaultValue
 * removal. Three checks:
 *   1. existence  — every literal key used in code exists in BOTH en-US and zh-CN.
 *   2. interp     — the {{var}} placeholder set matches between en-US and zh-CN.
 *   3. no-fallback — fail on any `t('key', '<string default>')` so a removed
 *                   inline fallback cannot be silently reintroduced.
 *
 * Key collection covers static t('literal', ...) plus enumerated i18n key fields
 * on object literals (labelKey/hintKey/titleKey only — not a generic *Key match,
 * which would wrongly pull in replaceKey and similar non-i18n fields).
 *
 * Dynamic keys (e.g. t(item.title) for menus, t(i18nKey, ...) from a URL param)
 * are runtime-resolved and skipped — they carry no string literal to check.
 *
 * Pass --allow-fallback to skip check 3 during migration (before the codemod).
 * Zero deps beyond `typescript`.
 */
const fs = require('node:fs');
const path = require('node:path');
const ts = require('typescript');

const ROOT = path.resolve(__dirname, '..');
const SRC = path.join(ROOT, 'src');
const LOCALE = path.join(SRC, 'locales');

const ALLOW_FALLBACK = process.argv.includes('--allow-fallback');
const ENUM_KEY_FIELDS = new Set(['labelKey', 'hintKey', 'titleKey']);

function loadTsDefault(entry) {
    const cache = new Map();
    const load = (file) => {
        if (cache.has(file)) return cache.get(file);
        const js = ts.transpileModule(fs.readFileSync(file, 'utf8'), {
            compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020, esModuleInterop: true },
        }).outputText;
        const mod = { exports: {} };
        cache.set(file, mod.exports);
        const dir = path.dirname(file);
        const req = (spec) => {
            if (!spec.startsWith('.')) return require(spec);
            let p = path.resolve(dir, spec);
            if (fs.existsSync(p) && fs.statSync(p).isDirectory()) p = path.join(p, 'index.ts');
            else if (!p.endsWith('.ts')) p += '.ts';
            return load(p);
        };
        new Function('require', 'module', 'exports', js)(req, mod, mod.exports);
        cache.set(file, mod.exports);
        return mod.exports;
    };
    const m = load(entry);
    return m.default || m;
}

function flatten(obj, prefix, out) {
    for (const [k, v] of Object.entries(obj)) {
        const key = prefix ? `${prefix}.${k}` : k;
        if (v && typeof v === 'object' && !Array.isArray(v)) flatten(v, key, out);
        else out[key] = v;
    }
    return out;
}

function placeholders(val) {
    const set = new Set();
    if (typeof val !== 'string') return set;
    for (const m of val.matchAll(/\{\{\s*([\w]+)\s*\}\}/g)) set.add(m[1]);
    return set;
}

function walk(dir, acc = []) {
    for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
        const p = path.join(dir, e.name);
        if (e.isDirectory()) {
            if (/[\\/]services$/.test(p)) continue;
            if (p === LOCALE) continue;
            walk(p, acc);
        } else if (/\.(ts|tsx)$/.test(e.name) && !/\.test\.(ts|tsx)$/.test(e.name)) {
            acc.push(p);
        }
    }
    return acc;
}

const usedKeys = new Map();
const fallbackViolations = [];

for (const file of walk(SRC)) {
    const sf = ts.createSourceFile(file, fs.readFileSync(file, 'utf8'), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
    const rel = path.relative(ROOT, file);
    const record = (key, node) => {
        if (!usedKeys.has(key)) {
            const { line } = sf.getLineAndCharacterOfPosition(node.getStart(sf));
            usedKeys.set(key, `${rel}:${line + 1}`);
        }
    };
    const visit = (node) => {
        if (ts.isCallExpression(node) && ts.isIdentifier(node.expression) && node.expression.text === 't') {
            const [a0, a1] = node.arguments;
            if (a0 && ts.isStringLiteralLike(a0)) {
                record(a0.text, a0);
                if (a1 && ts.isStringLiteral(a1)) {
                    const { line } = sf.getLineAndCharacterOfPosition(a1.getStart(sf));
                    fallbackViolations.push({ file: rel, line: line + 1, key: a0.text });
                }
            }
        }
        if (ts.isPropertyAssignment(node) && ts.isIdentifier(node.name)
            && ENUM_KEY_FIELDS.has(node.name.text) && ts.isStringLiteralLike(node.initializer)) {
            record(node.initializer.text, node.initializer);
        }
        ts.forEachChild(node, visit);
    };
    visit(sf);
}

const en = flatten(loadTsDefault(path.join(LOCALE, 'en-US.ts')), '', {});
const zh = flatten(loadTsDefault(path.join(LOCALE, 'zh-CN.ts')), '', {});

const missing = [];
for (const [key, loc] of usedKeys) {
    const inEn = key in en, inZh = key in zh;
    if (!inEn || !inZh) missing.push({ key, loc, where: !inEn && !inZh ? 'en+zh' : !inEn ? 'en' : 'zh' });
}

const interpMismatch = [];
for (const key of usedKeys.keys()) {
    if (!(key in en) || !(key in zh)) continue;
    const pe = placeholders(en[key]), pz = placeholders(zh[key]);
    const diff = [...pe].filter((v) => !pz.has(v)).concat([...pz].filter((v) => !pe.has(v)));
    if (diff.length) interpMismatch.push({ key, en: [...pe], zh: [...pz] });
}

let failed = false;
console.log(`[verify-i18n] web keys collected: ${usedKeys.size} | en: ${Object.keys(en).length} | zh: ${Object.keys(zh).length}`);

if (missing.length) {
    failed = true;
    console.error(`\n✗ [existence] ${missing.length} key(s) missing from locale:`);
    for (const m of missing) console.error(`    ${m.key}  (${m.where})  ${m.loc}`);
} else {
    console.log('✓ [existence] all keys present in en ∩ zh');
}

if (interpMismatch.length) {
    failed = true;
    console.error(`\n✗ [interp] ${interpMismatch.length} key(s) with mismatched {{vars}}:`);
    for (const m of interpMismatch) console.error(`    ${m.key}  en={${m.en}} zh={${m.zh}}`);
} else {
    console.log('✓ [interp] en/zh placeholder sets match');
}

if (fallbackViolations.length) {
    if (ALLOW_FALLBACK) {
        console.log(`⚠ [no-fallback] ${fallbackViolations.length} inline fallback(s) present (allowed during migration)`);
    } else {
        failed = true;
        console.error(`\n✗ [no-fallback] ${fallbackViolations.length} inline t(key,'default') still present:`);
        for (const v of fallbackViolations.slice(0, 20)) console.error(`    ${v.key}  ${v.file}:${v.line}`);
        if (fallbackViolations.length > 20) console.error(`    … and ${fallbackViolations.length - 20} more`);
    }
} else {
    console.log('✓ [no-fallback] no inline defaults');
}

process.exit(failed ? 1 : 0);
