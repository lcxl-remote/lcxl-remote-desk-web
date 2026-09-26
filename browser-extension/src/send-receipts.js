// Receipt coordination is shared by every connection in one extension worker.
export function createSendReceiptCoordinator({ read, write, execute, pageFromAction }) {
    const inflight = new Map();
    let writeTail = Promise.resolve();
    let admissionTail = Promise.resolve();

    function admit(operation) {
        const pending = admissionTail.then(operation);
        admissionTail = pending.catch(() => {});
        return pending;
    }

    function matches(receipt, identity) {
        return receipt?.idempotency_key === identity.idempotency_key &&
            receipt.snapshot_id === identity.snapshot_id &&
            receipt.snapshot_sha256 === identity.snapshot_sha256;
    }

    function remember(receipt) {
        const pending = writeTail.then(async () => {
            const stored = await read();
            const entries = Object.entries(stored)
                .filter(([key]) => key !== receipt.idempotency_key)
                .sort((left, right) => Number(left[1]?.observed_at_unix_ms || 0) - Number(right[1]?.observed_at_unix_ms || 0))
                .slice(-127);
            await write({ ...Object.fromEntries(entries), [receipt.idempotency_key]: receipt });
        });
        writeTail = pending.catch(() => {});
        return pending;
    }

    async function lookup(action, guard) {
        guard();
        const identity = {
            idempotency_key: action.activation_class.idempotency_key,
            snapshot_id: action.activation_class.snapshot_id,
            snapshot_sha256: action.activation_class.payload_sha256
        };
        const cached = (await read())[identity.idempotency_key];
        guard();
        if (cached) {
            if (!matches(cached, identity)) throw new Error("invalid_cached_send_receipt");
            return { pending: Promise.resolve({ page: pageFromAction(action), send_receipt: cached }) };
        }
        const existing = inflight.get(identity.idempotency_key);
        if (existing) {
            if (!matches(existing.identity, identity)) throw new Error("conflicting_inflight_send");
            return { pending: existing.pending };
        }
        const entry = { identity };
        entry.pending = (async () => {
            const result = await execute(action, guard);
            if (!matches(result?.send_receipt, identity)) throw new Error("invalid_send_receipt");
            await remember(result.send_receipt);
            return result;
        })().finally(() => admit(() => {
            if (inflight.get(identity.idempotency_key) === entry) inflight.delete(identity.idempotency_key);
        }));
        inflight.set(identity.idempotency_key, entry);
        return { pending: entry.pending };
    }

    return async (action, guard) => {
        // Serialize storage lookup with admission and retirement, not execution.
        const entry = await admit(() => lookup(action, guard));
        return entry.pending;
    };
}
