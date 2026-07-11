import { describe, it, expect } from 'vitest';

import { toSelect, fromSelect, CAPABILITY_PRESETS } from './capability-ceiling-editor';

describe('capability ceiling three-state mapping', () => {
    it('maps a boolean/null ceiling value to a select value', () => {
        expect(toSelect(true)).toBe('allow');
        expect(toSelect(false)).toBe('deny');
        expect(toSelect(null)).toBe('prompt');
        expect(toSelect(undefined)).toBe('prompt');
    });

    it('maps a select value back to the ceiling value', () => {
        expect(fromSelect('allow')).toBe(true);
        expect(fromSelect('deny')).toBe(false);
        expect(fromSelect('prompt')).toBeNull();
    });

    it('round-trips every three-state value', () => {
        for (const v of [true, false, null] as const) {
            expect(fromSelect(toSelect(v))).toBe(v);
        }
    });
});

describe('capability presets', () => {
    const dims = [
        'allow_remote_control',
        'allow_clipboard_sync',
        'allow_private_screen',
        'allow_whiteboard',
        'allow_terminal',
        'allow_file_browse',
        'allow_file_transfer',
    ] as const;

    it('view-only denies every grantable dimension', () => {
        for (const d of dims) expect(CAPABILITY_PRESETS.viewOnly[d]).toBe(false);
    });

    it('full allows every dimension', () => {
        for (const d of dims) expect(CAPABILITY_PRESETS.full[d]).toBe(true);
    });

    it('assist grants common support capabilities but withholds terminal', () => {
        expect(CAPABILITY_PRESETS.assist.allow_remote_control).toBe(true);
        expect(CAPABILITY_PRESETS.assist.allow_clipboard_sync).toBe(true);
        expect(CAPABILITY_PRESETS.assist.allow_file_browse).toBe(true);
        expect(CAPABILITY_PRESETS.assist.allow_terminal).toBe(false);
        expect(CAPABILITY_PRESETS.assist.allow_private_screen).toBeNull();
    });
});
