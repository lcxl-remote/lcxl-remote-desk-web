import type { GetModelMetricsOverviewQueryParams, MetricsCalls, MetricsGroups, MetricsOverview, MetricsSummary, QueryCoverage } from '@/services/types';
import { csvCell } from './format';

type Cell = string | number | boolean | null | undefined;
type Row = Record<string, Cell>;
type Query = GetModelMetricsOverviewQueryParams;

const COVERAGE_FIELDS = ['requested_from', 'requested_to', 'effective_from', 'effective_to', 'as_of', 'available_from', 'not_collected_before', 'trimmed_before', 'sample_status', 'cohort_basis', 'granularity', 'usage_source', 'live_pagination'] as const;
const FILTER_FIELDS = ['from', 'to', 'granularity', 'provider_id', 'model_id', 'surface', 'purpose', 'origin', 'tool', 'error', 'contract_revision', 'include_probe', 'group_sort', 'record_kind', 'outcome', 'min_duration_ms', 'latency', 'permission', 'dispatched'] as const;
const SUMMARY_FIELDS = ['row_type', 'group_key', 'provider_id', 'model_id', 'model_name', 'tool', 'metric', 'count', 'numerator', 'denominator', 'value', 'metric_sample_status', 'definition_version', 'denominator_scope', 'numerator_error', 'applied_filters', 'unsupported_filters', 'reason', 'sample_count', 'sum', 'average_ms', 'min_ms', 'max_ms', 'p50_ms', 'p95_ms', 'p99_ms', 'overflow_count', 'extrema_unavailable', 'estimated_percentiles', 'schema_path', 'stage', 'stage_outcome', 'configuration_revisions', 'configurations_limited'];
// Association IDs and user/device/session identities are deliberately excluded.
const RECORD_FIELDS = ['kind', 'started_at', 'updated_at', 'provider_id', 'model_id', 'model_name', 'purpose', 'surface', 'origin', 'configuration_scope', 'configuration_revision', 'contract_revision', 'protocol', 'outcome', 'not_started_reason', 'output', 'tool', 'ordinal', 'input_conclusion', 'input_issue', 'schema_path', 'permission', 'correction_status', 'correction_input', 'duration_ms', 'headers_ms', 'first_content_ms', 'http_status', 'input_tokens', 'output_tokens', 'cache_read_tokens', 'cache_write_tokens', 'usage_complete', 'generated_tool_count', 'tool_count', 'input_rejected_count', 'tool_counts_status', 'dispatched', 'detail_trimmed'] as const;

function context(coverage: QueryCoverage, query: Query, scope: string): Row {
    return {
        ...Object.fromEntries(COVERAGE_FIELDS.map((field) => [field, coverage[field]])),
        export_scope: scope,
        filters: JSON.stringify(Object.fromEntries(FILTER_FIELDS.flatMap((field) => query[field] == null ? [] : [[field, query[field]]]))),
    };
}

function csv(fields: readonly string[], rows: Row[]): string {
    return '\uFEFF' + [fields, ...rows.map((row) => fields.map((field) => row[field] == null ? '' : String(row[field])))].map((row) => row.map(csvCell).join(',')).join('\r\n');
}

function summaryRows(summary: MetricsSummary, group: Row = {}): Row[] {
    return [
        ...summary.counts.map((item) => ({ ...group, row_type: 'count', metric: item.key, count: item.count })),
        ...summary.rates.map((item) => ({ ...group, row_type: 'rate', metric: item.key, ...item, key: undefined, metric_sample_status: item.sample_status, applied_filters: item.applied_filters.join(';'), unsupported_filters: item.unsupported_filters.join(';') })),
        ...summary.quantities.map((item) => ({ ...group, row_type: 'quantity', metric: item.key, sample_count: item.sample_count, sum: item.sum })),
        ...summary.errors.map((item) => ({ ...group, row_type: 'error_count', metric: item.metric, numerator_error: item.error, count: item.count })),
        ...summary.schema_paths.map((item) => ({ ...group, row_type: 'schema_path', schema_path: item.path, count: item.count })),
        { ...group, row_type: 'schema_path_other', count: summary.other_schema_errors, reason: summary.schema_paths_limited ? 'path_budget' : '' },
        ...summary.stages.map((item) => ({ ...group, row_type: 'stage', stage: item.stage, stage_outcome: item.outcome, count: item.count })),
        ...[{ kind: 'duration', summary: summary.duration }, { kind: 'first_content', summary: summary.first_content }, ...summary.other_duration].map((item) => ({ ...group, row_type: 'latency', metric: item.kind, ...item.summary })),
    ];
}

export function aggregateCsv(value: MetricsOverview | MetricsGroups, query: Query): string {
    const base = context(value.coverage, query, 'current_filtered_aggregates');
    const rows = 'summary' in value ? summaryRows(value.summary) : [
        ...value.groups.flatMap((group) => [
            ...group.configurations.map((model) => ({ row_type: 'configuration', group_key: group.key, provider_id: model.provider_id, model_id: model.model_id, model_name: model.model_name, configuration_revisions: model.revisions.join(';'), configurations_limited: group.configurations_limited })),
            ...summaryRows(group.summary, { group_key: group.key, provider_id: group.provider_id, model_id: group.model_id, model_name: group.model_name, tool: group.tool }),
            ...group.associated_models.map((model) => ({ row_type: 'associated_model', group_key: group.key, tool: group.tool, provider_id: model.provider_id, model_id: model.model_id, model_name: model.model_name, count: model.tool_inputs, metric: 'tool_inputs' })),
            ...(group.other_model_count !== '0' ? [{ row_type: 'other_models', group_key: group.key, tool: group.tool, count: group.other_model_count, metric: 'model_count' }] : []),
        ]),
        ...(value.other ? summaryRows(value.other, { group_key: 'other' }) : []),
    ];
    return csv([...COVERAGE_FIELDS, 'export_scope', 'filters', ...SUMMARY_FIELDS], [{ ...base, row_type: 'context' }, ...rows.map((row) => ({ ...row, ...base }))]);
}

export function callsCsv(value: MetricsCalls, query: Query): string {
    const base = { ...context(value.coverage, query, 'current_call_page'), received_before: value.received_before, page_record_count: String(value.records.length), has_next_page: value.next_cursor != null };
    const rows: Row[] = value.records.map((record, index) => ({
        ...base, row_type: 'record', page_row: index + 1,
        ...Object.fromEntries(RECORD_FIELDS.map((field) => [field, record[field]])),
        stages: record.stages.map((stage) => `${stage.stage}:${stage.outcome}`).join(';'),
    }));
    return csv([...COVERAGE_FIELDS, 'export_scope', 'filters', 'received_before', 'page_record_count', 'has_next_page', 'row_type', 'page_row', ...RECORD_FIELDS, 'stages'], [{ ...base, row_type: 'context' }, ...rows]);
}

export function downloadMetricsCsv(value: string, filename: string) {
    const url = URL.createObjectURL(new Blob([value], { type: 'text/csv;charset=utf-8' }));
    const link = document.createElement('a');
    link.href = url; link.download = filename;
    document.body.appendChild(link);
    try { link.click(); } finally {
        link.remove();
        // Keep the blob available while the browser starts its download.
        window.setTimeout(() => URL.revokeObjectURL(url), 60000);
    }
}
