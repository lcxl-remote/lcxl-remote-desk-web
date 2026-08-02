#!/usr/bin/env node

const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const src = path.resolve(__dirname, '..', 'src');
function readTree(dir) {
    return fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
        const target = path.join(dir, entry.name);
        if (entry.isDirectory()) return entry.name === 'services' ? [] : readTree(target);
        return /\.(?:ts|tsx)$/.test(entry.name) ? [fs.readFileSync(target, 'utf8')] : [];
    });
}
const source = readTree(src).join('\n');
for (const legacy of [
    '/api/currentUser',
    '/api/login/outLogin',
    '/api/login/account',
    '/api/login/tauri',
    '/api/desk/api/login/password',
    'UserResponeCurrentUser',
    'UserResponeNoLogintUser',
]) {
    assert.ok(!source.includes(legacy), `web auth contract still references ${legacy}`);
}

const login = fs.readFileSync(path.join(src, 'features/auth/login-page.tsx'), 'utf8');
assert.ok(!/from ["']axios["']/.test(login), 'Tauri login must not import raw axios');
assert.match(login, /\buseLoginTauri\b/);
assert.ok(!/response\.data\.status/.test(login), 'Tauri login must read LoginOutcomeDto');

for (const file of [
    'features/settings/user-settings.tsx',
    'features/layout/app-sidebar.tsx',
    'features/auth/require-auth.tsx',
    'features/auth/login-page.tsx',
]) {
    const text = fs.readFileSync(path.join(src, file), 'utf8');
    assert.match(text, /\buseGetCurrentUser\b/, `${file} is not on the generated current-user hook`);
}

console.log('verify-auth-contract: web canonical consumers and Tauri login scan passed');
