/**
 * The set of file transfers this client currently owns.
 *
 * Two problems it solves, both of which showed up as a progress bar frozen at
 * 0% with no way out:
 *
 * 1. **Only the initiator may create a transfer.** Inbound messages carry a
 *    `transfer_id`, and the handlers used to build state from whatever id
 *    arrived. A reply landing after the transfer was already given up on would
 *    recreate its sink and put the row back into `transferring`. Handlers now
 *    ask this registry first, and an id it does not know is dropped. Ids are
 *    UUIDs and never reused, so "not registered" unambiguously means "already
 *    settled, or never ours" — no tombstones to keep or expire.
 *
 * 2. **A transfer that goes quiet must end.** A host can stop mid-stream, or
 *    never answer at all, and nothing in the protocol announces that. Each
 *    entry can arm an inactivity timer that fires after `timeoutMs` without a
 *    `touch`, letting the owner settle the transfer instead of waiting
 *    forever. Watching the whole stream rather than only the first reply is
 *    deliberate: a host that answers and then stalls is exactly as stuck.
 */

/**
 * How long a watched transfer may go without inbound activity.
 *
 * Matches the upload accept gate so both directions give up on the same
 * schedule.
 */
export const TRANSFER_INACTIVITY_TIMEOUT_MS = 30_000;

interface Entry {
    timer: ReturnType<typeof setTimeout> | null;
    onInactive: (() => void) | null;
}

export class TransferRegistry {
    private readonly entries = new Map<string, Entry>();
    private readonly timeoutMs: number;

    constructor(timeoutMs: number = TRANSFER_INACTIVITY_TIMEOUT_MS) {
        this.timeoutMs = timeoutMs;
    }

    /**
     * Register a transfer this client is starting. Re-registering a live id
     * leaves it — and any armed timer — alone.
     */
    start(transferId: string): void {
        if (!this.entries.has(transferId)) {
            this.entries.set(transferId, { timer: null, onInactive: null });
        }
    }

    isActive(transferId: string): boolean {
        return this.entries.has(transferId);
    }

    /**
     * Start (or replace) the inactivity timer for an active transfer.
     * `onInactive` runs at most once and never after the transfer settles.
     * Unknown ids are ignored.
     *
     * The transfer is still registered when `onInactive` runs, so the callback
     * is what must `settle` it — that way the owner's own teardown runs
     * through the same single exit as every other ending, instead of the
     * registry quietly dropping the entry and leaving a sink open.
     */
    watch(transferId: string, onInactive: () => void): void {
        const entry = this.entries.get(transferId);
        if (!entry) return;
        entry.onInactive = onInactive;
        this.arm(transferId, entry);
    }

    /**
     * Record inbound activity. Returns `false` when the transfer is not
     * active, which callers must treat as "drop this message".
     */
    touch(transferId: string): boolean {
        const entry = this.entries.get(transferId);
        if (!entry) return false;
        if (entry.onInactive) this.arm(transferId, entry);
        return true;
    }

    /**
     * End a transfer. Idempotent; returns `true` only for the call that
     * actually ended a live transfer, so the owner can run its cleanup once.
     */
    settle(transferId: string): boolean {
        const entry = this.entries.get(transferId);
        if (!entry) return false;
        this.clearTimer(entry);
        this.entries.delete(transferId);
        return true;
    }

    /** End every transfer, returning the ids that were still live. */
    settleAll(): string[] {
        const ids = [...this.entries.keys()];
        for (const entry of this.entries.values()) {
            this.clearTimer(entry);
        }
        this.entries.clear();
        return ids;
    }

    /** Live transfers. Exposed so leaks are assertable. */
    get activeCount(): number {
        return this.entries.size;
    }

    private arm(transferId: string, entry: Entry): void {
        this.clearTimer(entry);
        entry.timer = setTimeout(() => {
            // A timer belonging to a superseded entry must not speak for the
            // one now registered under the same id.
            if (this.entries.get(transferId) !== entry) return;
            // It has fired and cannot fire again, so drop the handle: the
            // entry is no longer armed while its owner tears it down.
            entry.timer = null;
            entry.onInactive?.();
        }, this.timeoutMs);
    }

    private clearTimer(entry: Entry): void {
        if (entry.timer !== null) {
            clearTimeout(entry.timer);
            entry.timer = null;
        }
    }
}
