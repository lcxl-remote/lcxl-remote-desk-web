import type { ScheduleSpec } from '@/services/types';

export function validTimezone(zone: string): boolean {
    try { new Intl.DateTimeFormat('en', { timeZone: zone }); return !!zone.trim(); } catch { return false; }
}
export function formatTime(at: string, zone: string, locale: string): string {
    return new Intl.DateTimeFormat(locale, { timeZone: zone, year: 'numeric', month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', timeZoneName: 'shortOffset' }).format(new Date(at));
}
export function ruleTimes(spec: ScheduleSpec, zone: string, locale: string, reference = new Date()): string[] {
    const rule = spec.rule;
    if (rule.kind === 'once') return [formatTime(rule.at, zone, locale)];
    if (rule.kind === 'interval' || rule.kind === 'after_confirmation') return [];
    const formatter = new Intl.DateTimeFormat(locale, { timeZone: zone, weekday: rule.kind === 'weekly' ? 'short' : undefined, hour: '2-digit', minute: '2-digit', second: '2-digit' });
    const base = new Date(reference.toISOString().slice(0, 10) + 'T' + rule.utc_time + 'Z');
    if (rule.kind === 'daily') return [formatter.format(base)];
    return Array.from({ length: 7 }, (_, index) => new Date(base.getTime() + index * 86_400_000))
        .filter(at => rule.weekdays.includes(at.getUTCDay() || 7)).map(at => formatter.format(at));
}

/** Display projection only. Unchanged forms retain the original UTC spec. */
export function projectRule(spec: ScheduleSpec, zone: string, reference = new Date()) {
    const rule = spec.rule.kind === 'after_confirmation'
        ? { kind: 'once' as const, at: new Date(reference.getTime() + spec.rule.delay_seconds * 1000).toISOString() } : spec.rule;
    const at = new Date(rule.kind === 'once' ? rule.at : rule.kind === 'interval' ? rule.anchor_at
        : reference.toISOString().slice(0, 10) + 'T' + rule.utc_time + 'Z');
    const parts = new Intl.DateTimeFormat('en-CA-u-ca-gregory-nu-latn', { timeZone: zone,
        year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', second: '2-digit', hourCycle: 'h23' }).formatToParts(at);
    const get = (type: string) => parts.find(part => part.type === type)!.value;
    const date = `${get('year')}-${get('month')}-${get('day')}`;
    const time = `${get('hour')}:${get('minute')}:${get('second')}`;
    const dayShift = Math.round((Date.parse(date + 'T00:00:00Z') - Date.parse(at.toISOString().slice(0, 10) + 'T00:00:00Z')) / 86_400_000);
    return { kind: rule.kind, date, time, days: rule.kind === 'weekly' ? rule.weekdays.map(day => (day - 1 + dayShift + 7) % 7 + 1).sort() : [1],
        seconds: rule.kind === 'interval' ? rule.every_seconds : 86400 };
}
