import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import {
    PairingRegistry,
    ReadActionBroker,
    validateInspectAction,
    validateInspectResult,
} from '../src/bridge-core.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const identity = {
    origin: 'https://localhost:32123',
    host: 'Excel',
    platform: 'PC',
    documentUrl: 'file:///C:/Users/owner/Documents/book.xlsx',
    requirements: { ExcelApi_1_1: true, ExcelApi_1_7: true },
};

function selection(overrides = {}) {
    return {
        status: 'Verified',
        value: {
            address: 'Sheet1!A1',
            rowCount: 1,
            columnCount: 1,
            formulas: [['=1+1']],
            values: [[2]],
            numberFormat: [['General']],
            ...overrides,
        },
    };
}

test('pairing is one-use, origin-bound, Excel-only, and expires idle sessions', () => {
    const registry = new PairingRegistry(identity.origin, { idleTtlMs: 5_000, maxTtlMs: 20_000 });
    const offer = registry.createOffer(10_000, 1_000);
    assert.throws(() => registry.redeem({ ...identity, origin: 'https://evil.example', code: offer.code }, 2_000), /origin_not_allowed/);
    assert.throws(() => registry.redeem({ ...identity, host: 'PowerPoint', code: offer.code }, 2_000), /office_identity_invalid/);
    assert.throws(() => registry.redeem({ ...identity, documentUrl: '', code: offer.code }, 2_000), /document_identity_invalid/);
    const session = registry.redeem({ ...identity, code: offer.code }, 2_000);
    assert.equal(registry.authenticate(session.sessionId, session.secret, 2_001).host, 'Excel');
    assert.throws(() => registry.redeem({ ...identity, code: offer.code }, 2_100), /pairing_offer_unavailable/);
    assert.throws(() => registry.authenticate(session.sessionId, session.secret, 7_002), /session_unauthorized/);
});

test('the protocol admits only a strictly shaped read action', () => {
    assert.deepEqual(validateInspectAction({ id: 'inspect-1', kind: 'inspect_selection', generation: 1 }), {
        id: 'inspect-1', kind: 'inspect_selection', generation: 1,
    });
    assert.throws(() => validateInspectAction({ id: 'write-1', kind: 'excel_set_formula', generation: 1 }), /action_kind_not_allowed/);
    assert.throws(() => validateInspectAction({ id: 'free-1', kind: 'run_javascript', generation: 1 }), /action_kind_not_allowed/);
    assert.throws(() => validateInspectAction({ id: 'inspect-1', kind: 'inspect_selection', generation: 1, formula: '=1' }), /action_kind_not_allowed/);
});

test('Excel projections reject oversized, ragged, nested, and extra data', () => {
    assert.deepEqual(validateInspectResult(selection()), selection());
    assert.deepEqual(
        validateInspectResult({ status: 'Failed', error: { code: 'selection_too_large' } }),
        { status: 'Failed', error: { code: 'selection_too_large' } },
    );
    assert.throws(
        () => validateInspectResult({ status: 'Failed', error: { code: 'arbitrary_office_error' } }),
        /result_failure_invalid/,
    );
    assert.equal(validateInspectResult(selection({ formulas: [[42]], values: [[42]] })).value.formulas[0][0], 42);
    assert.throws(() => validateInspectResult(selection({ rowCount: 5, columnCount: 5 })), /selection_size_invalid/);
    assert.throws(() => validateInspectResult(selection({ values: [[]] })), /matrix_shape_invalid/);
    assert.throws(() => validateInspectResult(selection({ values: [[{ secret: true }]] })), /cell_value_invalid/);
    assert.throws(() => validateInspectResult({ ...selection(), extra: true }), /result_status_invalid/);
});

test('completion must match the queued session, id, kind, and generation', () => {
    const broker = new ReadActionBroker();
    broker.addSession('session-a');
    broker.addSession('session-b');
    const action = broker.enqueue('session-a', { id: 'inspect-2', kind: 'inspect_selection', generation: 4 });
    assert.deepEqual(broker.next('session-a'), action);
    assert.throws(() => broker.complete('session-b', action.id, {
        kind: action.kind, generation: action.generation, result: selection(),
    }), /completion_without_request/);
    assert.throws(() => broker.complete('session-a', action.id, {
        kind: action.kind, generation: 5, result: selection(),
    }), /completion_correlation_invalid/);
    broker.complete('session-a', action.id, {
        kind: action.kind, generation: action.generation, result: selection(),
    });
    assert.equal(broker.getCompletion(action.id).result.status, 'Verified');
    assert.throws(() => broker.complete('session-a', action.id, {
        kind: action.kind, generation: action.generation, result: selection(),
    }), /completion_without_request/);

    const failed = broker.enqueue('session-a', { id: 'inspect-3', kind: 'inspect_selection', generation: 5 });
    broker.next('session-a');
    broker.complete('session-a', failed.id, {
        kind: failed.kind,
        generation: failed.generation,
        result: { status: 'Failed', error: { code: 'office_inspection_failed' } },
    });
    assert.equal(broker.getCompletion(failed.id).result.error.code, 'office_inspection_failed');
});

test('removing stale sessions also removes queued and inflight reads', () => {
    const broker = new ReadActionBroker();
    broker.addSession('active');
    broker.addSession('stale');
    broker.enqueue('stale', { id: 'inspect-stale', kind: 'inspect_selection', generation: 1 });
    broker.next('stale');
    broker.retainSessions(['active']);
    assert.throws(() => broker.enqueue('stale', {
        id: 'inspect-after-expiry', kind: 'inspect_selection', generation: 1,
    }), /session_not_found/);
    assert.throws(() => broker.complete('stale', 'inspect-stale', {
        kind: 'inspect_selection', generation: 1, result: selection(),
    }), /completion_without_request/);
});

test('the shipped task pane and manifest contain no write action surface', () => {
    const script = fs.readFileSync(path.join(root, 'public', 'taskpane.js'), 'utf8');
    const manifest = fs.readFileSync(path.join(root, 'manifest.xml'), 'utf8');
    const html = fs.readFileSync(path.join(root, 'public', 'taskpane.html'), 'utf8');
    assert.doesNotMatch(script, /excel_set_formula|powerpoint_|run_javascript/);
    assert.doesNotMatch(script, /\.formulas\s*=|\.values\s*=/);
    assert.match(manifest, /<Host Name="Workbook"\/>/);
    assert.doesNotMatch(manifest, /<Host Name="(?:Presentation|Document)"\/>/);
    assert.match(manifest, /<Permissions>ReadWriteDocument<\/Permissions>/);
    assert.match(html, /define no write action/);
});

test('runtime credentials are published only after the broker owns the port', () => {
    const source = fs.readFileSync(path.join(root, 'broker.mjs'), 'utf8');
    const listen = source.indexOf("server.listen(port, '127.0.0.1', () => {");
    const tokenWrite = source.indexOf('fs.writeFileSync(adminTokenPath', listen);
    const offerWrite = source.indexOf('writePairingOffer(offer);', listen);
    assert.ok(listen >= 0, 'broker listener callback must exist');
    assert.ok(tokenWrite > listen, 'admin token must be published after bind');
    assert.ok(offerWrite > listen, 'pairing offer must be published after bind');
    assert.equal(
        source.slice(0, listen).includes('fs.writeFileSync(adminTokenPath'),
        false,
        'a failed pre-bind startup must not touch the active admin token',
    );
});
