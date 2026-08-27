import crypto from 'node:crypto';
import fs from 'node:fs';
import https from 'node:https';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import {
    MAX_RESULT_BYTES,
    PairingRegistry,
    ReadActionBroker,
} from './src/bridge-core.mjs';

const root = path.dirname(fileURLToPath(import.meta.url));
const port = Number(process.env.OFFICE_BRIDGE_PORT ?? 32123);
const expectedOrigin = `https://localhost:${port}`;
const certPath = process.env.OFFICE_BRIDGE_CERT;
const keyPath = process.env.OFFICE_BRIDGE_KEY;
const adminTokenPath = process.env.OFFICE_BRIDGE_ADMIN_TOKEN_FILE;
const pairingOfferPath = process.env.OFFICE_BRIDGE_PAIRING_FILE;
const pairingTtlMs = Number(process.env.OFFICE_PAIRING_TTL_MS ?? 120_000);

if (!Number.isSafeInteger(port) || port < 1024 || port > 65535) throw new Error('OFFICE_BRIDGE_PORT is invalid.');
if (!certPath || !path.isAbsolute(certPath) || !keyPath || !path.isAbsolute(keyPath)) {
    throw new Error('OFFICE_BRIDGE_CERT and OFFICE_BRIDGE_KEY must be absolute trusted localhost TLS paths.');
}
if (!adminTokenPath || !path.isAbsolute(adminTokenPath)) {
    throw new Error('OFFICE_BRIDGE_ADMIN_TOKEN_FILE must be an absolute path owned by the local host.');
}
if (pairingOfferPath && !path.isAbsolute(pairingOfferPath)) {
    throw new Error('OFFICE_BRIDGE_PAIRING_FILE must be an absolute path.');
}

const registry = new PairingRegistry(expectedOrigin);
const actions = new ReadActionBroker();
const adminToken = crypto.randomBytes(32).toString('base64url');

function writePairingOffer(offer) {
    if (!pairingOfferPath) return;
    fs.writeFileSync(pairingOfferPath, JSON.stringify(offer), { encoding: 'utf8', mode: 0o600 });
}

function clearPairingOffer() {
    if (!pairingOfferPath) return;
    try {
        fs.unlinkSync(pairingOfferPath);
    } catch (error) {
        if (error.code !== 'ENOENT') throw error;
    }
}

const offer = registry.createOffer(pairingTtlMs);

function sendJson(response, status, body) {
    const data = Buffer.from(JSON.stringify(body));
    response.writeHead(status, {
        'content-type': 'application/json; charset=utf-8',
        'content-length': data.length,
        'cache-control': 'no-store',
        'x-content-type-options': 'nosniff',
    });
    response.end(data);
}

async function readJson(request, maxBytes = 64 * 1024) {
    if (request.headers['content-type']?.split(';')[0].trim() !== 'application/json') {
        throw new Error('content_type_invalid');
    }
    const chunks = [];
    let size = 0;
    for await (const chunk of request) {
        size += chunk.length;
        if (size > maxBytes) throw new Error('payload_too_large');
        chunks.push(chunk);
    }
    try {
        return JSON.parse(Buffer.concat(chunks).toString('utf8'));
    } catch {
        throw new Error('json_invalid');
    }
}

function bearer(request) {
    const value = request.headers.authorization ?? '';
    return value.startsWith('Bearer ') ? value.slice(7) : '';
}

function requireAdmin(request) {
    const supplied = bearer(request);
    const left = crypto.createHash('sha256').update(supplied).digest();
    const right = crypto.createHash('sha256').update(adminToken).digest();
    if (!crypto.timingSafeEqual(left, right)) throw new Error('admin_unauthorized');
}

function requireSession(request) {
    const sessionId = request.headers['x-office-session'];
    if (typeof sessionId !== 'string') throw new Error('session_unauthorized');
    try {
        const session = registry.authenticate(sessionId, bearer(request));
        return { sessionId, session };
    } catch (error) {
        actions.removeSession(sessionId);
        throw error;
    }
}

function serveStatic(request, response) {
    const pathname = new URL(request.url, expectedOrigin).pathname;
    if (pathname === '/icon.png') {
        const body = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=', 'base64');
        response.writeHead(200, { 'content-type': 'image/png', 'content-length': body.length, 'cache-control': 'public, max-age=3600' });
        response.end(body);
        return true;
    }
    const names = new Map([
        ['/', 'taskpane.html'],
        ['/taskpane.html', 'taskpane.html'],
        ['/taskpane.js', 'taskpane.js'],
        ['/taskpane.css', 'taskpane.css'],
    ]);
    const name = names.get(pathname);
    if (!name) return false;
    const body = fs.readFileSync(path.join(root, 'public', name));
    const contentType = name.endsWith('.html') ? 'text/html; charset=utf-8'
        : name.endsWith('.css') ? 'text/css; charset=utf-8'
            : 'text/javascript; charset=utf-8';
    response.writeHead(200, {
        'content-type': contentType,
        'content-length': body.length,
        'content-security-policy': "default-src 'none'; script-src 'self' https://appsforoffice.microsoft.com; connect-src 'self'; style-src 'self'; img-src 'self'; frame-ancestors https://*.officeapps.live.com https://*.office.com",
        'referrer-policy': 'no-referrer',
        'cache-control': 'no-store',
        'x-content-type-options': 'nosniff',
    });
    response.end(body);
    return true;
}

function statusFor(error) {
    if (error.message.endsWith('_unauthorized')) return 401;
    if (error.message === 'not_found' || error.message === 'session_not_found') return 404;
    if (error.message === 'action_queue_full' || error.message === 'action_id_conflict') return 409;
    return 400;
}

const server = https.createServer({
    cert: fs.readFileSync(certPath),
    key: fs.readFileSync(keyPath),
    minVersion: 'TLSv1.2',
}, async (request, response) => {
    try {
        const url = new URL(request.url, expectedOrigin);
        if (request.method === 'GET' && serveStatic(request, response)) return;
        if (request.method === 'POST' && url.pathname === '/v1/pair') {
            const body = await readJson(request);
            const session = registry.redeem({
                origin: request.headers.origin ?? '',
                code: body.code,
                host: body.host,
                platform: body.platform,
                documentUrl: body.documentUrl,
                requirements: body.requirements,
            });
            actions.addSession(session.sessionId);
            clearPairingOffer();
            sendJson(response, 200, session);
            return;
        }
        if (request.method === 'GET' && url.pathname === '/v1/actions/next') {
            const { sessionId } = requireSession(request);
            const action = actions.next(sessionId);
            if (!action) {
                response.writeHead(204, { 'cache-control': 'no-store' });
                response.end();
                return;
            }
            sendJson(response, 200, action);
            return;
        }
        if (request.method === 'POST' && url.pathname === '/v1/session/revoke') {
            const { sessionId } = requireSession(request);
            registry.revoke(sessionId);
            actions.removeSession(sessionId);
            sendJson(response, 200, { revoked: true });
            return;
        }
        if (request.method === 'POST' && url.pathname.startsWith('/v1/actions/') && url.pathname.endsWith('/completed')) {
            const { sessionId } = requireSession(request);
            const id = url.pathname.split('/')[3];
            const body = await readJson(request, MAX_RESULT_BYTES);
            actions.complete(sessionId, id, body);
            sendJson(response, 200, { accepted: true });
            return;
        }
        if (request.method === 'GET' && url.pathname === '/admin/pairing') {
            requireAdmin(request);
            sendJson(response, 200, { offer: registry.currentOffer() });
            return;
        }
        if (request.method === 'POST' && url.pathname === '/admin/pairing') {
            requireAdmin(request);
            const nextOffer = registry.createOffer(pairingTtlMs);
            writePairingOffer(nextOffer);
            sendJson(response, 201, { offer: nextOffer });
            return;
        }
        if (request.method === 'GET' && url.pathname === '/admin/sessions') {
            requireAdmin(request);
            const sessions = registry.listSessions();
            actions.retainSessions(sessions.map((session) => session.sessionId));
            sendJson(response, 200, {
                sessions: sessions.map((session) => ({
                    ...session,
                    pendingActions: actions.pendingCount(session.sessionId),
                })),
            });
            return;
        }
        if (request.method === 'POST' && url.pathname === '/admin/actions') {
            requireAdmin(request);
            const body = await readJson(request);
            const action = actions.enqueue(body.sessionId, body.action);
            sendJson(response, 202, action);
            return;
        }
        if (request.method === 'GET' && url.pathname.startsWith('/admin/actions/')) {
            requireAdmin(request);
            const id = url.pathname.split('/')[3];
            sendJson(response, 200, actions.getCompletion(id) ?? { stage: 'Pending' });
            return;
        }
        throw new Error('not_found');
    } catch (error) {
        sendJson(response, statusFor(error), { error: error.message });
    }
});

server.listen(port, '127.0.0.1', () => {
    // Publish credentials only after the listener owns the port. A second
    // process that loses an EADDRINUSE race must never overwrite the active
    // broker's admin token or pairing offer.
    fs.writeFileSync(adminTokenPath, adminToken, { encoding: 'utf8', mode: 0o600 });
    writePairingOffer(offer);
    process.stdout.write(`${JSON.stringify({
        port,
        expectedOrigin,
        pairingCode: offer.code,
        pairingExpiresAt: offer.expiresAt,
        pairingOfferFile: pairingOfferPath ?? null,
        adminTokenFile: adminTokenPath,
        capability: 'office.document.inspect',
    })}\n`);
});

function shutdown() {
    clearPairingOffer();
    server.close(() => process.exit(0));
}

for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, shutdown);
