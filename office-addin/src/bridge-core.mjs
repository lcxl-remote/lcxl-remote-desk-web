import crypto from 'node:crypto';

export const ACTION_KIND = 'inspect_selection';
export const MAX_EXCEL_CELLS = 16;
export const MAX_RESULT_BYTES = 256 * 1024;
export const MAX_PENDING_ACTIONS = 4;
export const DEFAULT_SESSION_IDLE_TTL_MS = 15_000;
export const DEFAULT_SESSION_MAX_TTL_MS = 8 * 60 * 60 * 1000;

const MAX_ACTION_ID_BYTES = 80;
const MAX_ADDRESS_BYTES = 512;
const MAX_FORMULA_BYTES = 512;
const MAX_CELL_TEXT_BYTES = 4 * 1024;
const MAX_NUMBER_FORMAT_BYTES = 128;
const EXCEL_REQUIREMENT_KEYS = Object.freeze(['ExcelApi_1_1', 'ExcelApi_1_7']);

function randomToken(bytes = 32) {
    return crypto.randomBytes(bytes).toString('base64url');
}

function digest(value) {
    return crypto.createHash('sha256').update(value, 'utf8').digest();
}

function equalSecret(left, right) {
    if (typeof left !== 'string' || typeof right !== 'string') return false;
    return crypto.timingSafeEqual(digest(left), digest(right));
}

function exactKeys(value, expected) {
    const keys = Object.keys(value).sort();
    const allowed = [...expected].sort();
    return keys.length === allowed.length && keys.every((key, index) => key === allowed[index]);
}

function boundedString(value, maxBytes, error) {
    if (typeof value !== 'string' || Buffer.byteLength(value, 'utf8') > maxBytes) {
        throw new Error(error);
    }
    return value;
}

export class PairingRegistry {
    #expectedOrigin;
    #idleTtlMs;
    #maxTtlMs;
    #offer;
    #sessions = new Map();

    constructor(expectedOrigin, options = {}) {
        this.#expectedOrigin = new URL(expectedOrigin).origin;
        this.#idleTtlMs = options.idleTtlMs ?? DEFAULT_SESSION_IDLE_TTL_MS;
        this.#maxTtlMs = options.maxTtlMs ?? DEFAULT_SESSION_MAX_TTL_MS;
        if (!Number.isSafeInteger(this.#idleTtlMs) || this.#idleTtlMs < 5_000 ||
            !Number.isSafeInteger(this.#maxTtlMs) || this.#maxTtlMs < this.#idleTtlMs) {
            throw new Error('session_ttl_invalid');
        }
    }

    createOffer(ttlMs = 120_000, now = Date.now()) {
        if (!Number.isSafeInteger(ttlMs) || ttlMs < 10_000 || ttlMs > 600_000) {
            throw new Error('pairing_ttl_invalid');
        }
        this.#offer = {
            code: crypto.randomInt(100_000, 1_000_000).toString(),
            expiresAt: now + ttlMs,
            redeemed: false,
        };
        return this.currentOffer(now);
    }

    currentOffer(now = Date.now()) {
        if (!this.#offer || this.#offer.redeemed || now > this.#offer.expiresAt) return null;
        return { code: this.#offer.code, expiresAt: this.#offer.expiresAt };
    }

    redeem({ origin, code, host, platform, documentUrl, requirements }, now = Date.now()) {
        if (typeof origin !== 'string' || new URL(origin).origin !== this.#expectedOrigin) {
            throw new Error('origin_not_allowed');
        }
        if (!this.#offer || this.#offer.redeemed || now > this.#offer.expiresAt) {
            throw new Error('pairing_offer_unavailable');
        }
        if (!equalSecret(code, this.#offer.code)) throw new Error('pairing_code_invalid');
        if (host !== 'Excel' || typeof platform !== 'string' || platform.length === 0 || platform.length > 32) {
            throw new Error('office_identity_invalid');
        }
        boundedString(documentUrl, 4 * 1024, 'document_identity_invalid');
        if (documentUrl.length === 0) throw new Error('document_identity_invalid');
        if (!requirements || typeof requirements !== 'object' ||
            !exactKeys(requirements, EXCEL_REQUIREMENT_KEYS)) {
            throw new Error('requirements_invalid');
        }
        const boundedRequirements = Object.fromEntries(EXCEL_REQUIREMENT_KEYS.map((key) => {
            if (typeof requirements[key] !== 'boolean') throw new Error('requirements_invalid');
            return [key, requirements[key]];
        }));
        if (!boundedRequirements.ExcelApi_1_1) throw new Error('excel_api_not_supported');

        this.#offer.redeemed = true;
        const sessionId = randomToken(18);
        const secret = randomToken();
        this.#sessions.set(sessionId, {
            secretHash: digest(secret),
            host,
            platform,
            documentUrlHash: digest(documentUrl).toString('hex'),
            requirements: boundedRequirements,
            createdAt: now,
            lastSeenAt: now,
        });
        return { sessionId, secret, host };
    }

    authenticate(sessionId, secret, now = Date.now(), touch = true) {
        const session = this.#sessions.get(sessionId);
        if (!session || now - session.lastSeenAt > this.#idleTtlMs ||
            now - session.createdAt > this.#maxTtlMs ||
            !crypto.timingSafeEqual(session.secretHash, digest(String(secret)))) {
            this.#sessions.delete(sessionId);
            throw new Error('session_unauthorized');
        }
        if (touch) session.lastSeenAt = now;
        return session;
    }

    revoke(sessionId) {
        this.#sessions.delete(sessionId);
    }

    listSessions(now = Date.now()) {
        this.prune(now);
        return [...this.#sessions.entries()].map(([sessionId, session]) => ({
            sessionId,
            host: session.host,
            platform: session.platform,
            documentUrlHash: session.documentUrlHash,
            requirements: session.requirements,
            createdAt: session.createdAt,
            lastSeenAt: session.lastSeenAt,
        }));
    }

    prune(now = Date.now()) {
        for (const [sessionId, session] of this.#sessions) {
            if (now - session.lastSeenAt > this.#idleTtlMs || now - session.createdAt > this.#maxTtlMs) {
                this.#sessions.delete(sessionId);
            }
        }
    }
}

export function validateInspectAction(value) {
    if (!value || typeof value !== 'object' || !exactKeys(value, ['generation', 'id', 'kind']) ||
        value.kind !== ACTION_KIND) {
        throw new Error('action_kind_not_allowed');
    }
    const id = boundedString(value.id, MAX_ACTION_ID_BYTES, 'action_id_invalid');
    if (id.length === 0 || !/^[a-zA-Z0-9_-]+$/.test(id)) throw new Error('action_id_invalid');
    if (!Number.isSafeInteger(value.generation) || value.generation <= 0) {
        throw new Error('action_generation_invalid');
    }
    return { id, kind: ACTION_KIND, generation: value.generation };
}

function validateScalar(value) {
    if (value === null || typeof value === 'boolean') return value;
    if (typeof value === 'number' && Number.isFinite(value)) return value;
    if (typeof value === 'string') return boundedString(value, MAX_CELL_TEXT_BYTES, 'cell_value_too_large');
    throw new Error('cell_value_invalid');
}

function validateMatrix(matrix, rows, columns, validateCell) {
    if (!Array.isArray(matrix) || matrix.length !== rows) throw new Error('matrix_shape_invalid');
    return matrix.map((row) => {
        if (!Array.isArray(row) || row.length !== columns) throw new Error('matrix_shape_invalid');
        return row.map(validateCell);
    });
}

export function validateInspectResult(value) {
    if (!value || typeof value !== 'object') {
        throw new Error('result_status_invalid');
    }
    if (value.status === 'Failed') {
        if (!exactKeys(value, ['error', 'status']) || !value.error ||
            typeof value.error !== 'object' || !exactKeys(value.error, ['code']) ||
            !['selection_too_large', 'office_inspection_failed'].includes(value.error.code)) {
            throw new Error('result_failure_invalid');
        }
        return { status: 'Failed', error: { code: value.error.code } };
    }
    if (!exactKeys(value, ['status', 'value']) || value.status !== 'Verified') {
        throw new Error('result_status_invalid');
    }
    const selection = value.value;
    const keys = ['address', 'columnCount', 'formulas', 'numberFormat', 'rowCount', 'values'];
    if (!selection || typeof selection !== 'object' || !exactKeys(selection, keys)) {
        throw new Error('selection_schema_invalid');
    }
    if (!Number.isSafeInteger(selection.rowCount) || selection.rowCount <= 0 ||
        !Number.isSafeInteger(selection.columnCount) || selection.columnCount <= 0 ||
        selection.rowCount * selection.columnCount > MAX_EXCEL_CELLS) {
        throw new Error('selection_size_invalid');
    }
    const rows = selection.rowCount;
    const columns = selection.columnCount;
    const result = {
        status: 'Verified',
        value: {
            address: boundedString(selection.address, MAX_ADDRESS_BYTES, 'selection_address_invalid'),
            rowCount: rows,
            columnCount: columns,
            formulas: validateMatrix(selection.formulas, rows, columns, (cell) => {
                if (cell === null || typeof cell === 'boolean') return cell;
                if (typeof cell === 'number' && Number.isFinite(cell)) return cell;
                return boundedString(cell, MAX_FORMULA_BYTES, 'formula_invalid');
            }),
            values: validateMatrix(selection.values, rows, columns, validateScalar),
            numberFormat: validateMatrix(selection.numberFormat, rows, columns, (cell) => {
                if (cell === null) return null;
                return boundedString(cell, MAX_NUMBER_FORMAT_BYTES, 'number_format_invalid');
            }),
        },
    };
    if (Buffer.byteLength(JSON.stringify(result), 'utf8') > MAX_RESULT_BYTES) {
        throw new Error('result_too_large');
    }
    return result;
}

export class ReadActionBroker {
    #queues = new Map();
    #inflight = new Map();
    #completions = new Map();

    addSession(sessionId) {
        this.#queues.set(sessionId, []);
    }

    removeSession(sessionId) {
        this.#queues.delete(sessionId);
        for (const [id, item] of this.#inflight) {
            if (item.sessionId === sessionId) this.#inflight.delete(id);
        }
    }

    retainSessions(activeSessionIds) {
        const active = new Set(activeSessionIds);
        for (const sessionId of this.#queues.keys()) {
            if (!active.has(sessionId)) this.removeSession(sessionId);
        }
    }

    enqueue(sessionId, input) {
        const action = validateInspectAction(input);
        const queue = this.#queues.get(sessionId);
        if (!queue) throw new Error('session_not_found');
        if (queue.length >= MAX_PENDING_ACTIONS) throw new Error('action_queue_full');
        if (this.#inflight.has(action.id) || this.#completions.has(action.id) ||
            [...this.#queues.values()].some((items) => items.some((item) => item.id === action.id))) {
            throw new Error('action_id_conflict');
        }
        queue.push(action);
        return action;
    }

    next(sessionId) {
        const queue = this.#queues.get(sessionId);
        if (!queue) throw new Error('session_not_found');
        const action = queue.shift();
        if (action) this.#inflight.set(action.id, { sessionId, action });
        return action;
    }

    complete(sessionId, id, body) {
        const pending = this.#inflight.get(id);
        if (!pending || pending.sessionId !== sessionId) throw new Error('completion_without_request');
        if (!body || typeof body !== 'object' || !exactKeys(body, ['generation', 'kind', 'result']) ||
            body.kind !== pending.action.kind || body.generation !== pending.action.generation) {
            throw new Error('completion_correlation_invalid');
        }
        const result = validateInspectResult(body.result);
        const completion = { action: pending.action, result };
        this.#inflight.delete(id);
        this.#completions.set(id, completion);
        return completion;
    }

    getCompletion(id) {
        return this.#completions.get(id);
    }

    pendingCount(sessionId) {
        return this.#queues.get(sessionId)?.length ?? 0;
    }
}
