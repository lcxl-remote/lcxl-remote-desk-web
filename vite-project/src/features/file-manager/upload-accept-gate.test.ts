import { afterEach, describe, expect, it, vi } from 'vitest';
import { createAcceptGate } from './upload-accept-gate';

describe('createAcceptGate', () => {
    afterEach(() => {
        vi.useRealTimers();
    });

    it('resolves the waiter when the transfer is accepted', async () => {
        const gate = createAcceptGate();
        const waited = gate.wait('t1');
        gate.accept('t1');
        await expect(waited).resolves.toBeUndefined();
    });

    it('rejects the waiter when the transfer is rejected with a message', async () => {
        const gate = createAcceptGate();
        const waited = gate.wait('t1');
        gate.reject('t1', 'directory not found');
        await expect(waited).rejects.toThrow('directory not found');
    });

    it('rejectAll wakes every pending waiter', async () => {
        const gate = createAcceptGate();
        const a = gate.wait('a');
        const b = gate.wait('b');
        gate.rejectAll('connection closed');
        await expect(a).rejects.toThrow('connection closed');
        await expect(b).rejects.toThrow('connection closed');
    });

    it('reject only affects the named transfer, not others', async () => {
        const gate = createAcceptGate();
        const a = gate.wait('a');
        const b = gate.wait('b');
        gate.reject('a', 'gone');
        gate.accept('b');
        await expect(a).rejects.toThrow('gone');
        await expect(b).resolves.toBeUndefined();
    });

    it('rejects with a timeout when no reply arrives', async () => {
        vi.useFakeTimers();
        const gate = createAcceptGate();
        const waited = gate.wait('t1', 5_000);
        const assertion = expect(waited).rejects.toThrow(/timed out/i);
        await vi.advanceTimersByTimeAsync(5_000);
        await assertion;
    });

    it('accept/reject for an unknown transfer is a safe no-op', () => {
        const gate = createAcceptGate();
        expect(() => gate.accept('missing')).not.toThrow();
        expect(() => gate.reject('missing', 'x')).not.toThrow();
        expect(() => gate.clear('missing')).not.toThrow();
    });

    it('clear drops the waiter without settling and is idempotent', async () => {
        vi.useFakeTimers();
        const gate = createAcceptGate();
        const waited = gate.wait('t1', 5_000);
        const settled = vi.fn();
        waited.then(settled, settled);
        gate.clear('t1');
        gate.clear('t1'); // idempotent
        // The timer was cleared, so advancing past the timeout must not settle.
        await vi.advanceTimersByTimeAsync(10_000);
        expect(settled).not.toHaveBeenCalled();
    });

    it('a second wait for the same id supersedes the first', async () => {
        const gate = createAcceptGate();
        const first = gate.wait('t1');
        const second = gate.wait('t1');
        gate.accept('t1');
        await expect(first).rejects.toThrow(/superseded/i);
        await expect(second).resolves.toBeUndefined();
    });
});
