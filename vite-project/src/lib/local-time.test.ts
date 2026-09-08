import { describe, expect, it } from 'vitest';
import { formatLocalTime } from './local-time';

describe('local timestamp display', () => {
    it('treats offset-free backend values as UTC and projects across date boundaries', () => {
        const expected = new Date('2026-09-07T01:00:00Z').toLocaleString('en-US', { timeZone: 'America/Los_Angeles' });
        expect(formatLocalTime('2026-09-07T01:00:00', 'en-US', 'America/Los_Angeles')).toBe(expected);
        expect(formatLocalTime('2026-09-07T09:00:00+08:00', 'en-US', 'America/Los_Angeles')).toBe(expected);
        expect(formatLocalTime('2026-09-07T01:00:00Z', 'en-US', 'America/Los_Angeles')).toBe(expected);
    });
    it('does not display an invalid timestamp as raw protocol text', () => {
        expect(formatLocalTime('invalid')).toBe('—');
    });
});
