import { describe, it, expect } from 'vitest';
import { getKeyboardShortcuts } from './keyboard-shortcuts';

describe('getKeyboardShortcuts', () => {
    it('returns the Command-based set for a macOS host', () => {
        const ids = getKeyboardShortcuts('Mac').map(s => s.id);
        expect(ids).toContain('forceQuit');
        expect(ids).toContain('spotlight');
        // Windows-only entries must not leak into the macOS menu.
        expect(ids).not.toContain('ctrlAltDel');
        expect(ids).not.toContain('winD');
    });

    it('falls back to the Windows set for non-macOS and unknown hosts', () => {
        for (const os of ['Windows', 'Linux', 'Other', undefined] as const) {
            const ids = getKeyboardShortcuts(os).map(s => s.id);
            expect(ids).toContain('ctrlAltDel');
            expect(ids).not.toContain('forceQuit');
        }
    });

    it('emits a chord as press-in-order then release-in-reverse', () => {
        // Force Quit = Cmd(91) + Option(18) + Esc(27).
        const forceQuit = getKeyboardShortcuts('Mac').find(s => s.id === 'forceQuit')!;
        expect(forceQuit.events).toEqual([
            { event: 'keydown', keyCode: 91 },
            { event: 'keydown', keyCode: 18 },
            { event: 'keydown', keyCode: 27 },
            { event: 'keyup', keyCode: 27 },
            { event: 'keyup', keyCode: 18 },
            { event: 'keyup', keyCode: 91 },
        ]);
    });

    it('gives every shortcut a unique id and an i18n label key', () => {
        for (const os of ['Mac', 'Windows'] as const) {
            const shortcuts = getKeyboardShortcuts(os);
            const ids = shortcuts.map(s => s.id);
            expect(new Set(ids).size).toBe(ids.length);
            for (const s of shortcuts) {
                expect(s.labelKey).toMatch(/^pages\.desk\.shortcut\./);
                expect(s.labelFallback.length).toBeGreaterThan(0);
            }
        }
    });
});
