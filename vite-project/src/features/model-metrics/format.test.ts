import { describe, expect, it } from 'vitest';
import { csvCell, decimalText, ERROR_SELECTORS, metricTime } from './format';

describe('model observation formatting', () => {
    it('displays timestamps in local time, including day changes and daylight saving', () => {
        const summer = metricTime('2026-10-08T00:00:00Z', 'en-US');
        const winter = metricTime('2026-01-08T00:00:00Z', 'en-US');
        expect(summer).not.toContain('T00:00:00Z');
        if (process.env.TZ === 'America/Los_Angeles') {
            expect(summer).toBe('10/07/2026, 17:00:00');
            expect(winter).toBe('01/07/2026, 16:00:00');
        } else if (process.env.TZ === 'Asia/Shanghai') {
            expect(summer).toBe('10/08/2026, 08:00:00');
            expect(winter).toBe('01/08/2026, 08:00:00');
        }
        expect(metricTime('2026-10-08T08:00:00+08:00', 'en-US')).toBe(summer);
        expect(metricTime(null)).toBe('—');
        expect(metricTime('invalid')).toBe('—');
    });

    it('displays only local date and time in both interface languages', () => {
        const timestamp = '2026-10-09T04:42:08Z';
        expect(metricTime(timestamp, 'zh-CN')).toMatch(/^\d{4}\/\d{2}\/\d{2} \d{2}:42:08$/);
        expect(metricTime(timestamp, 'en-US')).toMatch(/^\d{2}\/\d{2}\/\d{4}, \d{2}:42:08$/);
        if (process.env.TZ === 'Asia/Shanghai') {
            expect(metricTime(timestamp, 'zh-CN')).toBe('2026/10/09 12:42:08');
        }
    });

    it('retains integer precision and distinguishes unavailable values from zero', () => {
        expect(decimalText('900719925474099312345')).toBe('900,719,925,474,099,312,345');
        expect(decimalText('0')).toBe('0');
        expect(decimalText(null)).toBe('—');
        expect(decimalText(undefined)).toBe('—');
    });

    it('escapes spreadsheet formulas and quotes without losing decimal text', () => {
        expect(csvCell('=SUM(1,2)')).toBe('"\'=SUM(1,2)"');
        expect(csvCell('a"b')).toBe('"a""b"');
        expect(csvCell('9007199254740993')).toBe('"9007199254740993"');
        expect(csvCell('\n=SUM(1,2)')).toBe('"\'\n=SUM(1,2)"');
        expect(csvCell('  @SUM(1,2)')).toBe('"\'  @SUM(1,2)"');
    });

    it('provides separately named output and input protocol failures', () => {
        expect(ERROR_SELECTORS).toContain('output.invalid_protocol');
        expect(ERROR_SELECTORS).toContain('input.invalid_protocol');
        expect(new Set(ERROR_SELECTORS).size).toBe(ERROR_SELECTORS.length);
    });
});
