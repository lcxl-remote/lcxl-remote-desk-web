const status = document.querySelector('#status');
const pairingCode = document.querySelector('#pairing-code');
const pairButton = document.querySelector('#pair');
const stopButton = document.querySelector('#stop');
const MAX_EXCEL_CELLS = 16;
const MAX_RESULT_BYTES = 256 * 1024;
let session;
let pairedIdentity;
let stopped = false;

function show(value) {
    status.textContent = typeof value === 'string' ? value : JSON.stringify(value, null, 2);
}

function officeIdentity() {
    return {
        host: Office.context.host,
        platform: Office.context.platform,
        documentUrl: Office.context.document.url ?? '',
        requirements: {
            ExcelApi_1_1: Office.context.requirements.isSetSupported('ExcelApi', '1.1'),
            ExcelApi_1_7: Office.context.requirements.isSetSupported('ExcelApi', '1.7'),
        },
    };
}

async function readBody(response) {
    const body = await response.json();
    if (!response.ok) throw new Error(body.error ?? `bridge_http_${response.status}`);
    return body;
}

async function pair() {
    const identity = officeIdentity();
    if (identity.host !== Office.HostType.Excel) throw new Error('excel_host_required');
    if (!identity.documentUrl) throw new Error('save_document_before_pairing');
    if (!identity.requirements.ExcelApi_1_1) throw new Error('excel_api_1_1_required');
    const code = pairingCode.value.trim();
    if (!/^[0-9]{6}$/.test(code)) throw new Error('pairing_code_invalid');
    const response = await fetch('/v1/pair', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ code, ...identity }),
    });
    session = await readBody(response);
    pairedIdentity = identity;
    pairingCode.value = '';
    stopped = false;
    pairButton.disabled = true;
    stopButton.disabled = false;
    show({ paired: true, host: identity.host, platform: identity.platform, sessionId: session.sessionId });
    void poll();
}

async function disconnect(message) {
    const activeSession = session;
    stopped = true;
    session = undefined;
    pairedIdentity = undefined;
    pairButton.disabled = false;
    stopButton.disabled = true;
    if (activeSession) {
        try {
            await fetch('/v1/session/revoke', {
                method: 'POST',
                headers: {
                    'x-office-session': activeSession.sessionId,
                    authorization: `Bearer ${activeSession.secret}`,
                },
            });
        } catch {
            // Local credentials are discarded even when the old bridge is unreachable.
        }
    }
    show(message);
}

function validateAction(action) {
    const keys = Object.keys(action ?? {}).sort().join(',');
    if (keys !== 'generation,id,kind' || action.kind !== 'inspect_selection' ||
        typeof action.id !== 'string' || !/^[a-zA-Z0-9_-]{1,80}$/.test(action.id) ||
        !Number.isSafeInteger(action.generation) || action.generation <= 0) {
        throw new Error('action_not_allowed');
    }
    return action;
}

async function inspectExcelSelection() {
    return Excel.run(async (context) => {
        const range = context.workbook.getSelectedRange();
        range.load(['address', 'rowCount', 'columnCount']);
        await context.sync();
        if (range.rowCount <= 0 || range.columnCount <= 0 ||
            range.rowCount * range.columnCount > MAX_EXCEL_CELLS) {
            throw new Error(`selection_too_large: at most ${MAX_EXCEL_CELLS} cells`);
        }
        range.load(['formulas', 'values', 'numberFormat']);
        await context.sync();
        return {
            address: range.address,
            rowCount: range.rowCount,
            columnCount: range.columnCount,
            formulas: range.formulas,
            values: range.values,
            numberFormat: range.numberFormat,
        };
    });
}

async function complete(action, result) {
    const body = JSON.stringify({
        kind: action.kind,
        generation: action.generation,
        result,
    });
    if (new TextEncoder().encode(body).byteLength > MAX_RESULT_BYTES) throw new Error('result_too_large');
    const response = await fetch(`/v1/actions/${encodeURIComponent(action.id)}/completed`, {
        method: 'POST',
        headers: {
            'content-type': 'application/json',
            'x-office-session': session.sessionId,
            authorization: `Bearer ${session.secret}`,
        },
        body,
    });
    await readBody(response);
}

function inspectionFailure(error) {
    const code = typeof error?.message === 'string' && error.message.startsWith('selection_too_large:')
        ? 'selection_too_large'
        : 'office_inspection_failed';
    return { status: 'Failed', error: { code } };
}

async function poll() {
    while (session && !stopped) {
        try {
            const response = await fetch('/v1/actions/next', {
                headers: {
                    'x-office-session': session.sessionId,
                    authorization: `Bearer ${session.secret}`,
                },
            });
            if (response.status === 204) {
                await new Promise((resolve) => setTimeout(resolve, 750));
                continue;
            }
            const action = validateAction(await readBody(response));
            try {
                const result = await inspectExcelSelection();
                await complete(action, { status: 'Verified', value: result });
                show({ paired: true, lastInspection: { address: result.address, cells: result.rowCount * result.columnCount } });
            } catch (error) {
                await complete(action, inspectionFailure(error));
                show({ paired: true, error: error.message });
            }
        } catch (error) {
            if (error.message === 'session_unauthorized') {
                await disconnect('The local session expired. Pair this document again.');
                return;
            }
            show({ paired: Boolean(session), error: error.message });
            await new Promise((resolve) => setTimeout(resolve, 1500));
        }
    }
}

Office.onReady(() => {
    show(officeIdentity());
    pairButton.addEventListener('click', () => pair().catch((error) => show({ error: error.message })));
    stopButton.addEventListener('click', () => void disconnect('Local session revoked.'));
    setInterval(() => {
        if (!session || !pairedIdentity) return;
        try {
            const current = officeIdentity();
            if (current.host !== pairedIdentity.host || current.documentUrl !== pairedIdentity.documentUrl) {
                void disconnect('Document changed; pairing was revoked.');
            }
        } catch {
            void disconnect('Office identity became unavailable; pairing was revoked.');
        }
    }, 1000);
});
