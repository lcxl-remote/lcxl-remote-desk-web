/**
 * Per-transfer "wait for upload acceptance" gate.
 *
 * The file-transfer upload flow must not push binary chunks until the
 * host has confirmed it accepted the `upload_request` (the host opens
 * the destination file and replies `upload_response{accepted:true}`,
 * or refuses with a `transfer_error`). Previously the browser slept a
 * fixed 100 ms and streamed chunks regardless, racing the host's
 * decision and wasting bytes on a rejected transfer.
 *
 * This gate exposes a promise keyed by `transfer_id` that the upload
 * loop awaits. The data-channel control handler settles it:
 *
 * - `accept(id)`        — host accepted; the upload loop proceeds.
 * - `reject(id, msg)`   — host refused / a single transfer aborted
 *                         (cancel, manual remove, transfer_error).
 * - `rejectAll(reason)` — connection-wide failure (DC error, close);
 *                         wakes every pending waiter at once.
 *
 * A bounded timeout guards against a host that never answers: the
 * waiter rejects instead of hanging the UI in the "connecting" state
 * forever. All settle paths drop the map entry so a late duplicate
 * reply is a harmless no-op.
 */

/** Default time to wait for an `upload_response` before giving up. */
export const DEFAULT_ACCEPT_TIMEOUT_MS = 30_000;

interface PendingAccept {
    resolve: () => void;
    reject: (err: Error) => void;
    timer: ReturnType<typeof setTimeout> | null;
}

export interface UploadAcceptGate {
    /**
     * Wait until the host accepts (or refuses) the upload identified by
     * `transferId`. Resolves on `accept`, rejects on
     * `reject`/`rejectAll` or when `timeoutMs` elapses.
     */
    wait(transferId: string, timeoutMs?: number): Promise<void>;
    /** Resolve the waiter for `transferId` (host accepted). No-op if unknown. */
    accept(transferId: string): void;
    /** Reject the waiter for `transferId`. No-op if unknown. */
    reject(transferId: string, message: string): void;
    /** Reject every pending waiter (connection-wide failure). */
    rejectAll(reason: string): void;
    /** Drop a waiter without settling its promise (best-effort cleanup). */
    clear(transferId: string): void;
}

export function createAcceptGate(): UploadAcceptGate {
    const pending = new Map<string, PendingAccept>();

    const dispose = (transferId: string): PendingAccept | undefined => {
        const entry = pending.get(transferId);
        if (entry) {
            if (entry.timer !== null) clearTimeout(entry.timer);
            pending.delete(transferId);
        }
        return entry;
    };

    return {
        wait(transferId, timeoutMs = DEFAULT_ACCEPT_TIMEOUT_MS) {
            return new Promise<void>((resolve, reject) => {
                // A previous waiter for the same id (retry) is abandoned:
                // settle it as rejected so it cannot leak.
                dispose(transferId)?.reject(new Error('Upload superseded'));
                const timer =
                    timeoutMs > 0
                        ? setTimeout(() => {
                              dispose(transferId);
                              reject(new Error('Timed out waiting for upload acceptance'));
                          }, timeoutMs)
                        : null;
                pending.set(transferId, { resolve, reject, timer });
            });
        },
        accept(transferId) {
            dispose(transferId)?.resolve();
        },
        reject(transferId, message) {
            dispose(transferId)?.reject(new Error(message));
        },
        rejectAll(reason) {
            const entries = Array.from(pending.values());
            pending.clear();
            for (const entry of entries) {
                if (entry.timer !== null) clearTimeout(entry.timer);
                entry.reject(new Error(reason));
            }
        },
        clear(transferId) {
            dispose(transferId);
        },
    };
}
