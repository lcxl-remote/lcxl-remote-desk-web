import type { GetModelMetricsCallsQueryParams } from '@/services/types';

export type CallConditions = Pick<GetModelMetricsCallsQueryParams, 'record_kind' | 'outcome' | 'min_duration_ms' | 'latency' | 'permission' | 'dispatched'>;

const COUNTS: Record<string, CallConditions> = {
    calls: { record_kind: 'call' },
    returned: { record_kind: 'call', outcome: 'returned' },
    request_errors: { record_kind: 'call', outcome: 'request_error' },
    tools: { record_kind: 'tool' },
    input_rejected: { record_kind: 'tool', outcome: 'rejected' },
    permission_waiting: { record_kind: 'tool', permission: 'waiting' },
    operations_dispatched: { record_kind: 'operation', dispatched: true },
    operations_unknown: { record_kind: 'operation', outcome: 'unknown', dispatched: true },
};

export function countConditions(key: string): CallConditions | undefined {
    return COUNTS[key];
}

export function latencyConditions(key: string, minimum = 0): CallConditions | undefined {
    if (key === 'duration' || key === 'firstContent') return { record_kind: 'call', latency: key === 'firstContent' ? 'first_content' : 'duration', min_duration_ms: minimum };
    if (['attempt', 'tool', 'operation', 'runtime'].includes(key)) return { record_kind: key as CallConditions['record_kind'], latency: 'duration', min_duration_ms: minimum, ...(key === 'operation' ? { dispatched: true } : {}) };
    return undefined;
}

export function compatibleConditions(conditions: CallConditions, tool?: string | null): boolean {
    return !tool || conditions.record_kind === 'tool' || conditions.record_kind === 'operation';
}
