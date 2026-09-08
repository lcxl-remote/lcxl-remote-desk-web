/** Backend timestamps without an offset are UTC, never browser-local input. */
export function formatLocalTime(value: string, locale?: string, timeZone?: string): string {
    const normalized = /(?:Z|[+-]\d{2}:?\d{2})$/i.test(value) ? value : `${value}Z`;
    const date = new Date(normalized);
    if (!Number.isFinite(date.getTime())) return '—';
    return date.toLocaleString(locale, { timeZone });
}
