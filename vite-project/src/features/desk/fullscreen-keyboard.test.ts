import { describe, it, expect, vi } from 'vitest';
import { lockEscapeKey, unlockKeyboard } from './fullscreen-keyboard';

describe('lockEscapeKey', () => {
    it('locks the Escape key and resolves true when the API is available', async () => {
        const lock = vi.fn().mockResolvedValue(undefined);
        const ok = await lockEscapeKey({ keyboard: { lock } });
        expect(ok).toBe(true);
        expect(lock).toHaveBeenCalledWith(['Escape']);
    });

    it('resolves false (no-op) when the Keyboard Lock API is absent', async () => {
        expect(await lockEscapeKey({})).toBe(false);
        expect(await lockEscapeKey({ keyboard: {} })).toBe(false);
    });

    it('resolves false when the lock request is rejected', async () => {
        const lock = vi.fn().mockRejectedValue(new Error('not in fullscreen'));
        expect(await lockEscapeKey({ keyboard: { lock } })).toBe(false);
    });
});

describe('unlockKeyboard', () => {
    it('calls unlock when available', () => {
        const unlock = vi.fn();
        unlockKeyboard({ keyboard: { unlock } });
        expect(unlock).toHaveBeenCalledOnce();
    });

    it('does not throw when the API is absent', () => {
        expect(() => unlockKeyboard({})).not.toThrow();
        expect(() => unlockKeyboard({ keyboard: {} })).not.toThrow();
    });
});
