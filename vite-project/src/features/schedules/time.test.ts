import { describe, expect, it } from 'vitest';
import { formatTime, ruleTimes, validTimezone, projectRule } from './time';
import type { ScheduleSpec } from '@/services/types';

describe('fixed UTC schedule display', () => {
    it('converts both the clock and weekday without modifying the saved rule', () => {
        const spec: ScheduleSpec = { schema_version: 1, rule: { kind: 'weekly', weekdays: [7], utc_time: '17:00:00' } };
        const original = JSON.stringify(spec);
        expect(ruleTimes(spec, 'Asia/Shanghai', 'en-US', new Date('2026-09-06T00:00:00Z'))[0]).toMatch(/Mon/);
        expect(JSON.stringify(spec)).toBe(original);
    });
    it('keeps one UTC instant while local display changes across DST', () => {
        const first = formatTime('2026-11-01T08:30:00Z', 'America/Los_Angeles', 'en-US');
        const second = formatTime('2026-11-01T09:30:00Z', 'America/Los_Angeles', 'en-US');
        expect(first).not.toBe(second);
        expect(first).toContain("GMT-7");
        expect(second).toContain("GMT-8");
        expect(validTimezone('America/Los_Angeles')).toBe(true);
        expect(validTimezone('invalid-zone')).toBe(false);
    });
});

it('projects editable weekdays using one reference offset and preserves interval anchors', () => {
    const spec: ScheduleSpec = { schema_version: 1, rule: { kind: 'weekly', weekdays: [1, 7], utc_time: '20:30:00' } };
    expect(projectRule(spec, 'Asia/Kathmandu', new Date('2026-09-07T00:00:00Z'))).toMatchObject({
        kind: 'weekly', date: '2026-09-08', time: '02:15:00', days: [1, 2],
    });
    expect(projectRule({ schema_version: 1, rule: { kind: 'interval', anchor_at: '2026-11-01T09:30:00Z', every_seconds: 90 } },
        'America/Los_Angeles')).toMatchObject({ kind: 'interval', date: '2026-11-01', time: '01:30:00', seconds: 90 });
    expect(spec.rule).toEqual({ kind: 'weekly', weekdays: [1, 7], utc_time: '20:30:00' });
});
