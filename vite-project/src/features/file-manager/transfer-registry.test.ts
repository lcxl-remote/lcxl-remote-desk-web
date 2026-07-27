import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { TransferRegistry, TRANSFER_INACTIVITY_TIMEOUT_MS } from './transfer-registry';

describe('TransferRegistry', () => {
    beforeEach(() => {
        vi.useFakeTimers();
    });

    afterEach(() => {
        vi.useRealTimers();
    });

    it('only recognizes transfers it was told about', () => {
        const registry = new TransferRegistry();
        expect(registry.isActive('t1')).toBe(false);
        expect(registry.touch('t1')).toBe(false);

        registry.start('t1');
        expect(registry.isActive('t1')).toBe(true);
        expect(registry.touch('t1')).toBe(true);
    });

    it('settles once and stays settled', () => {
        const registry = new TransferRegistry();
        registry.start('t1');

        expect(registry.settle('t1')).toBe(true);
        expect(registry.settle('t1')).toBe(false);
        expect(registry.isActive('t1')).toBe(false);
        expect(registry.activeCount).toBe(0);
    });

    // The defect: a transfer that never hears back from the host.
    it('fires the inactivity callback when nothing ever arrives', () => {
        const registry = new TransferRegistry();
        const onInactive = vi.fn();
        registry.start('t1');
        registry.watch('t1', onInactive);

        vi.advanceTimersByTime(TRANSFER_INACTIVITY_TIMEOUT_MS - 1);
        expect(onInactive).not.toHaveBeenCalled();

        vi.advanceTimersByTime(1);
        expect(onInactive).toHaveBeenCalledTimes(1);
    });

    // The callback is the owner's single exit, so it must find the transfer
    // still registered — otherwise its teardown would be a silent no-op and
    // the sink would stay open.
    it('leaves the transfer settleable by its own callback', () => {
        const registry = new TransferRegistry();
        let settledFromCallback: boolean | null = null;
        registry.start('t1');
        registry.watch('t1', () => {
            settledFromCallback = registry.settle('t1');
        });

        vi.advanceTimersByTime(TRANSFER_INACTIVITY_TIMEOUT_MS);
        expect(settledFromCallback).toBe(true);
        expect(registry.isActive('t1')).toBe(false);
        expect(vi.getTimerCount()).toBe(0);
    });

    // A host that answers and then stops sending is just as stuck, so the
    // watchdog covers the whole stream rather than only the first reply.
    it('fires after a first reply that is never followed by data', () => {
        const registry = new TransferRegistry();
        const onInactive = vi.fn();
        registry.start('t1');
        registry.watch('t1', onInactive);

        vi.advanceTimersByTime(TRANSFER_INACTIVITY_TIMEOUT_MS - 1);
        expect(registry.touch('t1')).toBe(true);

        vi.advanceTimersByTime(TRANSFER_INACTIVITY_TIMEOUT_MS - 1);
        expect(onInactive).not.toHaveBeenCalled();
        vi.advanceTimersByTime(1);
        expect(onInactive).toHaveBeenCalledTimes(1);
    });

    it('fires when a stream stalls partway through', () => {
        const registry = new TransferRegistry();
        const onInactive = vi.fn();
        registry.start('t1');
        registry.watch('t1', onInactive);

        for (let chunk = 0; chunk < 5; chunk++) {
            vi.advanceTimersByTime(TRANSFER_INACTIVITY_TIMEOUT_MS / 2);
            expect(registry.touch('t1')).toBe(true);
        }
        expect(onInactive).not.toHaveBeenCalled();

        vi.advanceTimersByTime(TRANSFER_INACTIVITY_TIMEOUT_MS);
        expect(onInactive).toHaveBeenCalledTimes(1);
    });

    // Completion and the timeout can be in flight at the same moment;
    // whichever wins, the other must become a no-op.
    it('does not fire after the transfer completes', () => {
        const registry = new TransferRegistry();
        const onInactive = vi.fn();
        registry.start('t1');
        registry.watch('t1', onInactive);

        vi.advanceTimersByTime(TRANSFER_INACTIVITY_TIMEOUT_MS - 1);
        expect(registry.settle('t1')).toBe(true);

        vi.advanceTimersByTime(TRANSFER_INACTIVITY_TIMEOUT_MS * 2);
        expect(onInactive).not.toHaveBeenCalled();
    });

    it('lets only one of the timeout and the completion end the transfer', () => {
        const registry = new TransferRegistry();
        const settledBy: string[] = [];
        registry.start('t1');
        registry.watch('t1', () => {
            if (registry.settle('t1')) settledBy.push('timeout');
        });

        vi.advanceTimersByTime(TRANSFER_INACTIVITY_TIMEOUT_MS);
        // A completion arriving right behind the timeout finds nothing to end.
        if (registry.settle('t1')) settledBy.push('completion');
        expect(settledBy).toEqual(['timeout']);
    });

    it('drops messages that arrive after a timeout has settled the transfer', () => {
        const registry = new TransferRegistry();
        registry.start('t1');
        registry.watch('t1', () => {
            registry.settle('t1');
        });

        vi.advanceTimersByTime(TRANSFER_INACTIVITY_TIMEOUT_MS);
        expect(registry.touch('t1')).toBe(false);
        expect(registry.isActive('t1')).toBe(false);
    });

    it('leaves an unwatched transfer alone forever', () => {
        const registry = new TransferRegistry();
        registry.start('t1');

        vi.advanceTimersByTime(TRANSFER_INACTIVITY_TIMEOUT_MS * 10);
        expect(registry.isActive('t1')).toBe(true);
        expect(vi.getTimerCount()).toBe(0);
    });

    it('re-watching replaces the previous timer rather than adding one', () => {
        const registry = new TransferRegistry();
        const first = vi.fn();
        const second = vi.fn();
        registry.start('t1');
        registry.watch('t1', first);
        registry.watch('t1', second);
        expect(vi.getTimerCount()).toBe(1);

        vi.advanceTimersByTime(TRANSFER_INACTIVITY_TIMEOUT_MS);
        expect(first).not.toHaveBeenCalled();
        expect(second).toHaveBeenCalledTimes(1);
    });

    it('ignores watch and start for ids it does not own', () => {
        const registry = new TransferRegistry();
        const onInactive = vi.fn();
        registry.watch('never-started', onInactive);

        vi.advanceTimersByTime(TRANSFER_INACTIVITY_TIMEOUT_MS * 2);
        expect(onInactive).not.toHaveBeenCalled();
        expect(registry.activeCount).toBe(0);
    });

    it('settles everything at once and reports what was live', () => {
        const registry = new TransferRegistry();
        const onInactive = vi.fn();
        registry.start('t1');
        registry.start('t2');
        registry.watch('t1', onInactive);

        expect(registry.settleAll().sort()).toEqual(['t1', 't2']);
        expect(registry.activeCount).toBe(0);
        expect(registry.settleAll()).toEqual([]);

        vi.advanceTimersByTime(TRANSFER_INACTIVITY_TIMEOUT_MS * 2);
        expect(onInactive).not.toHaveBeenCalled();
    });

    // Every entry and every timer must be released, or a long-lived file
    // manager tab accumulates one of each per transfer.
    it('leaves nothing behind after many transfers', () => {
        const registry = new TransferRegistry();
        for (let i = 0; i < 500; i++) {
            const id = `t${i}`;
            registry.start(id);
            registry.watch(id, () => {});
            registry.touch(id);
            registry.settle(id);
        }

        expect(registry.activeCount).toBe(0);
        expect(vi.getTimerCount()).toBe(0);
    });

    // The loop above ends every transfer explicitly, which is the path a host
    // that answers takes. A host that goes quiet leaves the watchdog to end it
    // instead, and that path has to clear just as completely — an upload whose
    // completion is never acknowledged is otherwise one stranded entry each.
    it('leaves nothing behind when the watchdog is what ends the transfer', () => {
        const registry = new TransferRegistry();
        for (let i = 0; i < 500; i++) {
            const id = `t${i}`;
            registry.start(id);
            registry.watch(id, () => {
                registry.settle(id);
            });
        }

        vi.advanceTimersByTime(TRANSFER_INACTIVITY_TIMEOUT_MS);

        expect(registry.activeCount).toBe(0);
        expect(vi.getTimerCount()).toBe(0);
    });

    // Callers settle inside the callback so their own teardown runs, but a
    // caller that forgets must not be able to leak: the timer is spent, so
    // nothing else would ever release the entry, and it would go on answering
    // `touch` for a transfer nobody owns.
    it('releases an entry its callback left behind', () => {
        const registry = new TransferRegistry();
        registry.start('t1');
        registry.watch('t1', () => {});

        vi.advanceTimersByTime(TRANSFER_INACTIVITY_TIMEOUT_MS * 10);

        expect(registry.activeCount).toBe(0);
        expect(vi.getTimerCount()).toBe(0);
        expect(registry.touch('t1')).toBe(false);
    });

    // The callback still owns the release when it takes it, and the backstop
    // must not turn one ending into two.
    it('lets the callback do the settling when it does', () => {
        const registry = new TransferRegistry();
        let settledByCallback = false;
        registry.start('t1');
        registry.watch('t1', () => {
            settledByCallback = registry.settle('t1');
        });

        vi.advanceTimersByTime(TRANSFER_INACTIVITY_TIMEOUT_MS);

        expect(settledByCallback).toBe(true);
        expect(registry.settle('t1')).toBe(false);
        expect(registry.activeCount).toBe(0);
    });
});
