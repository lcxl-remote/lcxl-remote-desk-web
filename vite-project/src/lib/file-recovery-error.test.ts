import { describe, expect, it } from 'vitest';
import { RecoveryFailure, recoveryErrorKey, requireRecoveryZip } from './file-recovery-error';

describe('backup download failures', () => {
    it.each(['busy', 'identity_changed', 'clock_changed', 'material_unavailable', 'material_expired', 'material_cleaning', 'material_cleaned'])('preserves the %s category from a JSON download response', async reason => {
        const text = JSON.stringify({ success: false, data: reason });
        const blob = Object.assign(new Blob([text], { type: 'application/json' }), { text: async () => text });
        const failure = await requireRecoveryZip(blob).catch(error => error);
        expect(failure).toBeInstanceOf(RecoveryFailure);
        expect(recoveryErrorKey(failure)).toBe(`error.${reason}`);
    });
    it('does not expose arbitrary server messages or accept error JSON as a ZIP', async () => {
        const text = JSON.stringify({ success: false, data: '/private/internal-path', message: 'raw failure' });
        const blob = Object.assign(new Blob([text], { type: 'application/json' }), { text: async () => text });
        const failure = await requireRecoveryZip(blob).catch(error => error);
        expect(recoveryErrorKey(failure)).toBe('failed');
        expect(String(failure)).not.toContain('/private/');
    });
    it('returns the successful ZIP unchanged', async () => {
        const blob = new Blob(['PK-test'], { type: 'application/zip' });
        expect(await requireRecoveryZip(blob)).toBe(blob);
    });
});
