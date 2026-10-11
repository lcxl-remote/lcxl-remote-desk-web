import { describe, expect, it } from 'vitest';
import { getLocalKeyboardLabels } from './keyboard-labels';

describe('local keyboard shortcut labels', () => {
    it.each(['MacIntel', 'MacARM'])('uses Mac symbols and readable key names for %s', platform => {
        expect(getLocalKeyboardLabels({ platform, userAgent: 'Mozilla/5.0' })).toEqual({
            display: { enter: '↩', shiftEnter: '⇧↩', primaryEnter: '⌘↩' },
            description: { enter: 'Return', shiftEnter: 'Shift+Return', primaryEnter: 'Command+Return' },
        });
    });

    it.each(['Win32', 'Linux x86_64', 'iPhone', ''])('retains text labels for %s', platform => {
        const labels = { enter: 'Enter', shiftEnter: 'Shift+Enter', primaryEnter: 'Ctrl+Enter' };
        expect(getLocalKeyboardLabels({ platform, userAgent: 'Mozilla/5.0' })).toEqual({ display: labels, description: labels });
    });

    it('prefers client hints and falls back to the user agent when the platform is unavailable', () => {
        expect(getLocalKeyboardLabels({ platform: 'Win32', userAgent: 'Mozilla/5.0',
            userAgentData: { platform: 'macOS' } }).display.shiftEnter).toBe('⇧↩');
        expect(getLocalKeyboardLabels({ platform: '', userAgent: 'Mozilla/5.0 (Macintosh; Intel Mac OS X)' })
            .display.primaryEnter).toBe('⌘↩');
    });
});
