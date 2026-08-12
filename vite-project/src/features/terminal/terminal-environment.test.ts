import { describe, expect, it } from 'vitest';
import { terminalOs, terminalShell } from './terminal-environment';

describe('terminal environment', () => {
    it('uses the host-reported macOS platform even when the shell is bash', () => {
        expect(terminalShell('/bin/bash,-l')).toBe('bash');
        expect(terminalOs('Mac')).toBe('macos');
    });

    it('does not invent Linux when the host platform is unavailable', () => {
        expect(terminalShell('/bin/bash')).toBe('bash');
        expect(terminalOs(undefined)).toBe('unknown');
        expect(terminalOs('Other')).toBe('unknown');
    });

    it('normalizes every known desktop host platform', () => {
        expect(terminalOs('Windows')).toBe('windows');
        expect(terminalOs('Linux')).toBe('linux');
        expect(terminalOs('Mac')).toBe('macos');
    });
});
