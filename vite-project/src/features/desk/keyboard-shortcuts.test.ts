import { describe, it, expect } from 'vitest';
import { getKeyboardShortcuts } from './keyboard-shortcuts';

describe('getKeyboardShortcuts', () => {
    it('returns the Command-based set for a macOS host', () => {
        const ids = getKeyboardShortcuts('Mac').map(s => s.id);
        expect(ids).toContain('forceQuit');
        expect(ids).toContain('spotlight');
        expect(ids).toContain('switchInputSource');
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

    it('sends the macOS input-source shortcut as Control + Space', () => {
        const switchInputSource = getKeyboardShortcuts('Mac')
            .find(s => s.id === 'switchInputSource')!;
        expect(switchInputSource.events).toEqual([
            { event: 'keydown', keyCode: 17 },
            { event: 'keydown', keyCode: 32 },
            { event: 'keyup', keyCode: 32 },
            { event: 'keyup', keyCode: 17 },
        ]);
    });

    it('appends an Esc entry only when includeEscape is set', () => {
        for (const os of ['Mac', 'Windows', undefined] as const) {
            expect(getKeyboardShortcuts(os).some(s => s.id === 'escape')).toBe(false);
            const withEsc = getKeyboardShortcuts(os, { includeEscape: true });
            const esc = withEsc.find(s => s.id === 'escape');
            expect(esc).toBeDefined();
            // Esc is appended last and sends a bare Escape (VK 27).
            expect(withEsc[withEsc.length - 1].id).toBe('escape');
            expect(esc!.events).toEqual([
                { event: 'keydown', keyCode: 27 },
                { event: 'keyup', keyCode: 27 },
            ]);
        }
    });

    it('gives every shortcut a unique id and an i18n label key', () => {
        for (const os of ['Mac', 'Windows'] as const) {
            const shortcuts = getKeyboardShortcuts(os);
            const ids = shortcuts.map(s => s.id);
            expect(new Set(ids).size).toBe(ids.length);
            for (const s of shortcuts) {
                expect(s.labelKey).toMatch(/^pages\.desk\.shortcut\./);
                expect(s.events.length).toBeGreaterThan(0);
            }
        }
    });
});
