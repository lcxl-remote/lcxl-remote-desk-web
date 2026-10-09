export function metricTime(value?: string | null, locale?: string): string {
    if (!value) return '—';
    const date = new Date(value);
    if (!Number.isFinite(date.getTime())) return '—';
    return date.toLocaleString(locale, {
        year: 'numeric', month: '2-digit', day: '2-digit',
        hour: '2-digit', minute: '2-digit', second: '2-digit',
        hourCycle: 'h23',
    });
}

export function decimalText(value?: string | null): string {
    if (value == null) return '—';
    return /^\d+$/.test(value) ? value.replace(/\B(?=(\d{3})+(?!\d))/g, ',') : value;
}

export function sampleBand(value?: string | null): 'zero' | 'single' | 'small' | 'regular' | undefined {
    if (value == null || !/^\d+$/.test(value)) return undefined;
    const count = BigInt(value);
    return count === 0n ? 'zero' : count === 1n ? 'single' : count < 20n ? 'small' : 'regular';
}

export function observationText(value: string): string {
    return value.length > 48 ? `${value.slice(0, 12)}…${value.slice(-8)}` : value;
}

export function csvCell(value: string): string {
    const safe = /^\s*[=+\-@]|^[\t\r\n]/.test(value) ? `'${value}` : value;
    return `"${safe.replace(/"/g, '""')}"`;
}

export const ERROR_SELECTORS = [
    'request.http_error', 'request.provider_error', 'request.transport_error', 'request.timeout', 'request.stream_error',
    'output.invalid_protocol', 'output.invalid_structured_output', 'output.empty_response',
    'input.unknown_tool', 'input.unexposed_tool', 'input.invalid_protocol', 'input.invalid_json',
    'input.missing_field', 'input.unknown_field', 'input.type', 'input.enum', 'input.pattern', 'input.length',
    'input.combination', 'input.schema_unavailable', 'input.unknown_reference', 'input.reference_type',
    'input.reference_expired', 'input.reference_unavailable', 'input.semantic', 'input.precondition', 'input.unknown',
] as const;
