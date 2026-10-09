import { vi } from 'vitest';
import type { MetricsGroups, MetricsStatus, MetricsSummary, QueryCoverage, ObservationRecord } from '@/services/types';
import type { MetricsApi } from './dashboard';

export function coverage(state: QueryCoverage['sample_status'] = 'complete'): QueryCoverage {
    return {
        requested_from: '2026-10-08T00:00:00Z', requested_to: '2026-10-09T00:00:00Z',
        effective_from: '2026-10-08T00:00:00Z', effective_to: '2026-10-09T00:00:00Z',
        as_of: '2026-10-09T00:00:00Z', available_from: '2026-10-07T00:00:00Z',
        not_collected_before: state === 'not_collected' ? '2026-10-10T00:00:00Z' : null,
        trimmed_before: null, sample_status: state,
        cohort_basis: 'model_call_start;operation_dispatch;runtime_event;usage_metered',
        granularity: 'hour', usage_source: 'observations', usage_filter_dimensions: [], live_pagination: true,
    };
}

export function summary(): MetricsSummary {
    const latency = { count: '0', average_ms: null, min_ms: null, max_ms: null, p50_ms: null, p95_ms: null, p99_ms: null, overflow_count: '0', extrema_unavailable: false, estimated_percentiles: true };
    return {
        quantities: [], counts: [{ key: 'calls', count: '1' }, { key: 'returned', count: '1' }, { key: 'request_errors', count: '0' }],
        rates: [{ key: 'request_failure', numerator: '0', denominator: '1', value: 0, definition_version: 1, sample_status: 'complete', applied_filters: ['time'], unsupported_filters: [], denominator_scope: 'returned_or_known_request_error', numerator_error: null, reason: null }],
        duration: latency, first_content: latency, other_duration: [],
        errors: [], schema_paths: [], other_schema_errors: '0', schema_paths_limited: false, stages: [],
    };
}

export function groups(tool?: string): MetricsGroups {
    return {
        coverage: coverage(), other: null,
        groups: [{ key: 'provider-a:model-a', provider_id: 'provider-a', model_id: 'model-a', model_name: 'Same name', tool: tool ?? null, summary: summary(), associated_models: [], other_model_count: '0', configurations: [], configurations_limited: false }],
    };
}

export function recordFixture(): ObservationRecord {
    return {
        id: 'private-observation-id', call_id: 'private-call-id', tool_observation_id: 'private-input-id', kind: 'tool',
        started_at: '2026-10-08T00:00:00Z', updated_at: '2026-10-08T00:00:01Z',
        provider_id: 'provider-1', model_id: 'model-1', model_name: 'Selected model',
        purpose: 'agent', surface: 'assistant', origin: 'user', configuration_scope: 'local',
        configuration_revision: '9007199254740993', contract_revision: '1', protocol: 'open_ai_chat_completions',
        outcome: 'rejected', not_started_reason: null, output: null, tool: 'read_file', ordinal: 0,
        input_conclusion: 'rejected', input_issue: 'type', schema_path: '$.items[].count',
        stages: [{ stage: 'schema', outcome: 'failed' }], permission: 'not_reached',
        correction_of: 'private-correction-id', correction_status: 'linked', correction_input: 'accepted',
        correction_group_root: 'private-group-id', correction_group: null, correction_group_unavailable: false,
        duration_ms: 42, headers_ms: null, first_content_ms: null, http_status: null,
        input_tokens: '9007199254740993', output_tokens: null, cache_read_tokens: null, cache_write_tokens: null,
        usage_complete: null, generated_tool_count: null, tool_count: null, input_rejected_count: null, tool_counts_status: 'not_applicable', dispatched: null, detail_trimmed: false,
    };
}

export function apiFixture(): MetricsApi {
    const status: MetricsStatus = {
        state: 'ready', enabled: true, schema_version: 1, definition_version: 1,
        available_from: '2026-10-07T00:00:00Z', settings_revision: '1', settings_effective: true,
        last_persisted: null, last_aggregated: null, as_of: '2026-10-09T00:00:00Z',
        backlog: '0', oldest_pending: null, dropped_events: '0', discarded_events: '0', unassociated_records: '0',
        instrumented_surfaces: ['assistant'], unsupported_surfaces: [], gaps: [], retention: null, storage: null, reason: null,
    };
    return {
        status: vi.fn(async () => status),
        overview: vi.fn(async () => ({ coverage: coverage(), summary: summary(), previous: null })),
        series: vi.fn(async () => ({ coverage: coverage(), points: [] })),
        models: vi.fn(async () => groups()), tools: vi.fn(async () => groups('read_file')),
        calls: vi.fn(async () => ({ coverage: coverage(), records: [], next_cursor: null, received_before: '2026-10-09T00:00:00Z' })),
        unassociated: vi.fn(async () => ({ coverage: { ...coverage(), cohort_basis: 'fact_received' }, records: [], next_cursor: null })),
        runtime: vi.fn(async () => ({ coverage: coverage(), groups: [], other: null })),
        detail: vi.fn(async () => { throw new Error('No detail fixture'); }),
        settings: vi.fn(async () => { throw new Error('No settings fixture'); }),
        save: vi.fn(async (value) => value),
        accessError: (error) => !!error && typeof error === 'object' && 'fixtureAccessDenied' in error,
    };
}
