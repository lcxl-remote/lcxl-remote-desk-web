import { describe, it, expect } from 'vitest';
import {
    isValidRetentionDays,
    MIN_RETENTION_DAYS,
    MAX_RETENTION_DAYS,
} from './usage-retention-validation';

describe('isValidRetentionDays', () => {
    it('accepts the inclusive bounds and typical windows', () => {
        expect(isValidRetentionDays(MIN_RETENTION_DAYS)).toBe(true);
        expect(isValidRetentionDays(MAX_RETENTION_DAYS)).toBe(true);
        expect(isValidRetentionDays(30)).toBe(true);
    });

    it('rejects out-of-range, non-integer, and NaN values', () => {
        expect(isValidRetentionDays(0)).toBe(false);
        expect(isValidRetentionDays(MAX_RETENTION_DAYS + 1)).toBe(false);
        expect(isValidRetentionDays(7.5)).toBe(false);
        expect(isValidRetentionDays(Number.NaN)).toBe(false);
    });
});
