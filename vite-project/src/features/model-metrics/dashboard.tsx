import { useEffect, useMemo, useState, type ReactNode } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import type { GetModelMetricsOverviewQueryParams, GetModelMetricsRuntimeQueryParams, MetricsStatus, MetricsOverview, MetricsSeries, MetricsGroups, MetricsCalls, MetricsCallDetail, MetricsSettings, MetricsSummary, MetricsRuntimeGroups, MetricsUnassociated, GetModelMetricsUnassociatedQueryParams, LatencySummary } from '@/services/types';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { Label } from '@/components/ui/label';
import { MetricsRuntimePanel } from './runtime-panel';
import { ObservationDetails, RecordOutcome } from './record-detail';
import { decimalText, sampleBand } from './format';
import { aggregateCsv, callsCsv, downloadMetricsCsv } from './export';
import { MetricsFilters } from './filters';
import { CallFilters } from './call-filters';
import { MetricsComparison } from './comparison';
import { countConditions, latencyConditions, compatibleConditions, type CallConditions } from './drill-down';
import { MetricsCoverage } from './coverage';
import { UnassociatedFacts } from './unassociated';
import { Link } from 'react-router-dom';
import { MetricTime } from './time';
import { metricTime } from './format';
import { UsageRangePicker, presetRange, type UsageRangeParams } from '@/features/usage/usage-range-picker';

type Query = GetModelMetricsOverviewQueryParams;
export interface MetricsApi {
    status(signal: AbortSignal): Promise<MetricsStatus>;
    overview(query: Query, signal: AbortSignal): Promise<MetricsOverview>;
    series(query: Query, signal: AbortSignal): Promise<MetricsSeries>;
    models(query: Query, signal: AbortSignal): Promise<MetricsGroups>;
    tools(query: Query, signal: AbortSignal): Promise<MetricsGroups>;
    runtime(query: GetModelMetricsRuntimeQueryParams, signal: AbortSignal): Promise<MetricsRuntimeGroups>;
    calls(query: Query, signal: AbortSignal): Promise<MetricsCalls>;
    unassociated(query: GetModelMetricsUnassociatedQueryParams, signal: AbortSignal): Promise<MetricsUnassociated>;
    detail(id: string, signal: AbortSignal): Promise<MetricsCallDetail>;
    settings(signal: AbortSignal): Promise<MetricsSettings>;
    save(settings: MetricsSettings): Promise<MetricsSettings>;
    accessError(error: unknown): boolean;
}

function Summary({ value, previous, drill, tool }: { value: MetricsSummary; previous?: MetricsSummary | null; drill?: (conditions: CallConditions) => void; tool?: string | null }) {
    const { t } = useTranslation();
    const percentage = (value?: number | null) => value == null ? '—' : `${(value * 100).toFixed(2)}%`;
    const latencyGroups: [string, LatencySummary][] = [['duration', value.duration], ['firstContent', value.first_content], ...value.other_duration.map((item): [string, LatencySummary] => [item.kind, item.summary])];
    return <div className="space-y-5">
        <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-4">{value.counts.filter((item) => ['calls', 'returned', 'request_errors', 'tools', 'input_rejected', 'permission_waiting', 'operations_dispatched', 'operations_unknown'].includes(item.key)).map((item) => <Card key={item.key}><button type="button" className="w-full text-left rounded focus-visible:outline focus-visible:outline-2 disabled:cursor-default" aria-label={`${t('modelMetrics.drillDown')}: ${t(`modelMetrics.count.${item.key}`)}`} disabled={!drill || item.count === '0' || !compatibleConditions(countConditions(item.key)!, tool)} onClick={() => drill?.(countConditions(item.key)!)}><CardHeader className="pb-2"><CardDescription>{t(`modelMetrics.count.${item.key}`)}</CardDescription></CardHeader><CardContent className="text-2xl tabular-nums break-all">{decimalText(item.count)}</CardContent></button></Card>)}</div>
        <details><summary className="text-sm">{t('modelMetrics.allCounts')}</summary><dl className="grid gap-3 pt-3 sm:grid-cols-2 lg:grid-cols-3">{value.counts.map((item) => <div key={item.key} className="rounded border p-3 text-sm"><dt className="text-muted-foreground">{t(`modelMetrics.count.${item.key}`, { defaultValue: item.key })}</dt><dd className="tabular-nums">{decimalText(item.count)}</dd></div>)}</dl></details>
        {value.errors.length > 0 && <details><summary className="text-sm">{t('modelMetrics.errorDistribution')}</summary><div className="overflow-x-auto pt-3"><table className="w-full text-sm"><thead><tr className="text-left"><th>{t('modelMetrics.metric')}</th><th>{t('modelMetrics.filter.error')}</th><th>{t('modelMetrics.sample')}</th></tr></thead><tbody>{value.errors.map((error) => <tr key={`${error.metric}:${error.error}`}><td>{t(`modelMetrics.rate.${error.metric}`)}</td><td>{t(`modelMetrics.error.${error.error}`)}</td><td className="font-mono">{decimalText(error.count)}</td></tr>)}</tbody></table></div></details>}
        {value.stages.length > 0 && <details><summary className="text-sm">{t('modelMetrics.stageDistribution')}</summary><div className="overflow-x-auto pt-3"><table className="w-full text-sm"><thead><tr className="text-left"><th>{t('modelMetrics.stage')}</th><th>{t('modelMetrics.outcome')}</th><th>{t('modelMetrics.sample')}</th></tr></thead><tbody>{value.stages.map((stage) => <tr key={`${stage.stage}:${stage.outcome}`}><td>{t(`modelMetrics.value.stage.${stage.stage}`)}</td><td>{t(`modelMetrics.value.stageOutcome.${stage.outcome}`)}</td><td className="font-mono">{decimalText(stage.count)}</td></tr>)}</tbody></table></div></details>}
        {(value.schema_paths.length > 0 || value.other_schema_errors !== '0') && <Card><CardHeader><CardTitle className="text-base">{t('modelMetrics.schemaPaths')}</CardTitle><CardDescription>{t('modelMetrics.schemaPathsHint')}</CardDescription></CardHeader><CardContent><table className="w-full text-sm"><tbody>{value.schema_paths.map((path) => <tr key={path.path}><td className="break-all font-mono">{path.path}</td><td className="font-mono text-right">{decimalText(path.count)}</td></tr>)}<tr><td>{t('modelMetrics.other')}</td><td className="font-mono text-right">{decimalText(value.other_schema_errors)}</td></tr></tbody></table>{value.schema_paths_limited && <p role="status" className="pt-2 text-sm text-muted-foreground">{t('modelMetrics.schemaPathsLimited')}</p>}</CardContent></Card>}
        <div className="overflow-x-auto"><table className="w-full text-sm"><thead><tr className="border-b text-left"><th>{t('modelMetrics.metric')}</th><th>{t('modelMetrics.rate')}</th><th>{t('modelMetrics.sample')}</th><th>{t('modelMetrics.previous')}</th><th>{t('modelMetrics.state')}</th></tr></thead><tbody>{value.rates.map((item) => <tr key={item.key} className="border-b">
            <td className="py-3"><p>{t(`modelMetrics.rate.${item.key}`)}</p>
                {item.numerator_error && <p className="text-xs text-muted-foreground">{t('modelMetrics.selectedError', { error: t(`modelMetrics.error.${item.numerator_error}`) })}</p>}
                <p className="text-xs text-muted-foreground">{t('modelMetrics.denominatorScope', { scope: t(`modelMetrics.denominator.${item.denominator_scope}`) })}</p>
            </td><td>{percentage(item.value)}{sampleBand(item.denominator) === 'regular' && item.value != null && item.value >= 0.5 && ['request_failure', 'attempt_failure', 'output_rejection', 'tool_format_failure', 'reference_failure', 'input_rejection'].includes(item.key) && <p className="text-xs text-amber-700 dark:text-amber-400">{t('modelMetrics.highObservedErrors')}</p>}</td><td className="font-mono">{decimalText(item.numerator)} / {decimalText(item.denominator)}{['zero', 'single', 'small'].includes(sampleBand(item.denominator) ?? '') && <p className="text-xs text-muted-foreground font-sans">{t(`modelMetrics.sampleBand.${sampleBand(item.denominator)}`)}</p>}</td><td>{percentage(previous?.rates.find((row) => row.key === item.key)?.value)}</td>
            <td>{t(`modelMetrics.state.${item.sample_status}`)}{item.reason && <p className="text-xs text-muted-foreground">{t(`modelMetrics.reason.${item.reason}`)}</p>}{item.unsupported_filters.length > 0 && <p className="text-xs text-muted-foreground">{t('modelMetrics.unsupportedFilters', { filters: item.unsupported_filters.map((filter) => t(`modelMetrics.filter.${filter}`)).join(', ') })}</p>}</td>
        </tr>)}</tbody></table></div>
        <div className="grid gap-3 md:grid-cols-2">{latencyGroups.map(([key, latency]) => <Card key={key}><CardHeader><CardTitle className="text-base">{t(key === 'duration' || key === 'firstContent' ? `modelMetrics.${key}` : `modelMetrics.durationKind.${key}`, { defaultValue: key })}</CardTitle><CardDescription>{t('modelMetrics.latencySamples', { countText: latency.count })}</CardDescription></CardHeader><CardContent className="grid grid-cols-3 gap-2 text-sm">{[['average', latency.average_ms], ['p50', latency.p50_ms], ['p95', latency.p95_ms], ['p99', latency.p99_ms], ['min', latency.min_ms], ['max', latency.max_ms]].map(([name, number]) => <div key={String(name)}>{t(`modelMetrics.latency.${name}`)}<p className="font-mono">{number == null ? '—' : t('modelMetrics.milliseconds', { value: Math.round(Number(number)) })}</p></div>)}{latency.estimated_percentiles && <p className="col-span-3 text-muted-foreground">{t('modelMetrics.percentileHint')}</p>}{latency.extrema_unavailable && <p className="col-span-3 text-muted-foreground">{t('modelMetrics.extremaUnavailable')}</p>}{drill && latency.count !== '0' && latencyConditions(key) && compatibleConditions(latencyConditions(key)!, tool) && <div className="col-span-3 flex flex-wrap gap-2"><Button variant="outline" size="sm" onClick={() => drill(latencyConditions(key)!)}>{t('modelMetrics.viewLatencySamples')}</Button>{latency.p95_ms != null && latency.p95_ms <= 86400000 && <Button variant="outline" size="sm" onClick={() => drill(latencyConditions(key, latency.p95_ms!)!)}>{t('modelMetrics.viewSlowSamples')}</Button>}</div>}</CardContent></Card>)}</div>
    </div>;
}

function Storage({ value }: { value: NonNullable<MetricsStatus['storage']> }) {
    const { t } = useTranslation();
    return <details className="text-sm border-t pt-2"><summary>{t('modelMetrics.storage.title')}</summary>
        <div className="space-y-2 pt-2">
            <p>{t('modelMetrics.storage.charged')}: <span className="font-mono">{decimalText(value.charged_bytes)} / {decimalText(value.budget_bytes)}</span> {t('modelMetrics.storage.bytes')}</p>
            <p>{t('modelMetrics.storage.physical')}: <span className="font-mono">{decimalText(value.physical_allocated_bytes)}</span> {t('modelMetrics.storage.bytes')} · {t('modelMetrics.storage.sampledAt')}: <MetricTime value={value.physical_sampled_at}/></p>
            <p className="text-muted-foreground">{t('modelMetrics.storage.hint', { reserved: decimalText(value.reserved_bytes) })}</p>
            <div className="overflow-x-auto"><table className="w-full"><thead><tr className="text-left"><th>{t('modelMetrics.storage.object')}</th><th>{t('modelMetrics.storage.rows')}</th><th>{t('modelMetrics.storage.budget')}</th><th>{t('modelMetrics.storage.cleanup')}</th></tr></thead><tbody>{value.rows.map((row) => <tr key={row.kind}><td>{t(`modelMetrics.storage.kind.${row.kind}`)}</td><td className="font-mono">{decimalText(row.rows)}</td><td className="font-mono">{decimalText(row.budget)}</td><td>{row.cleanup_active || value.cleanup_active ? t('modelMetrics.storage.active') : '—'}</td></tr>)}</tbody></table></div>
            <p>{t('modelMetrics.storage.trimmedDetails')}: {decimalText(value.trimmed_details)} · {t('modelMetrics.storage.pendingDropped')}: {decimalText(value.dropped_pending_events)}</p>
            {value.frozen_before && <p>{t('modelMetrics.storage.frozenBefore')}: <MetricTime value={value.frozen_before}/></p>}
            {value.rollup_trim_before && <p>{t('modelMetrics.storage.trimmedBefore')}: <MetricTime value={value.rollup_trim_before}/></p>}
        </div>
    </details>;
}

function Trend({ value }: { value: MetricsSeries }) {
    const { t, i18n } = useTranslation();
    const [metric, setMetric] = useState('request_failure');
    const latency = metric === 'duration' || metric === 'first_content';
    const points = value.points.map((point) => {
        const rate = point.summary.rates.find((item) => item.key === metric);
        const timing = metric === 'duration' ? point.summary.duration : point.summary.first_content;
        return { bucket: point.bucket, number: latency ? timing.p95_ms : rate?.value, samples: latency ? timing.count : `${rate?.numerator ?? '—'} / ${rate?.denominator ?? '—'}`, state: latency ? value.coverage.sample_status : rate?.sample_status };
    });
    const maximum = latency ? Math.max(1, ...points.map((point) => point.number ?? 0)) : 1;
    const valid = points.flatMap((point, index) => point.number == null ? [] : [{ ...point, index, x: index * 1000 / Math.max(1, points.length - 1), y: 180 - point.number / maximum * 170 }]);
    const display = (number?: number | null) => number == null ? '—' : latency ? t('modelMetrics.milliseconds', { value: number }) : `${(number * 100).toFixed(2)}%`;
    return <Card><CardHeader><CardTitle>{t('modelMetrics.trend')}</CardTitle><select className="h-9 rounded border bg-background px-2" aria-label={t('modelMetrics.metric')} value={metric} onChange={(event) => setMetric(event.target.value)}>{value.points[0]?.summary.rates.map((rate) => <option key={rate.key} value={rate.key}>{t(`modelMetrics.rate.${rate.key}`, { defaultValue: rate.key })}</option>)}<option value="duration">{t('modelMetrics.durationP95')}</option><option value="first_content">{t('modelMetrics.firstContentP95')}</option></select></CardHeader><CardContent>{valid.length ? <svg viewBox="0 0 1000 200" className="w-full" role="img" aria-label={t('modelMetrics.trend')}><path d="M0 10 V180 H1000" fill="none" stroke="currentColor" opacity=".25"/>{valid.map((point, index) => <g key={point.bucket}>{index > 0 && valid[index - 1].index + 1 === point.index && <line x1={valid[index - 1].x} y1={valid[index - 1].y} x2={point.x} y2={point.y} stroke="currentColor"/>}<circle cx={point.x} cy={point.y} r="3" fill="currentColor"><title>{metricTime(point.bucket, i18n.language)}: {display(point.number)} · {point.samples}</title></circle></g>)}</svg> : <p>{t('modelMetrics.noSamples')}</p>}{latency && <p className="text-sm text-muted-foreground">{t('modelMetrics.percentileHint')}</p>}<details><summary>{t('modelMetrics.exactSamples')}</summary><div className="max-h-64 overflow-auto"><table className="w-full text-sm"><thead><tr className="text-left"><th>{t('modelMetrics.time')}</th><th>{t('modelMetrics.metric')}</th><th>{t('modelMetrics.sample')}</th><th>{t('modelMetrics.state')}</th></tr></thead><tbody>{points.map((point) => <tr key={point.bucket}><td><MetricTime value={point.bucket}/></td><td className="font-mono">{display(point.number)}</td><td className="font-mono">{point.samples}</td><td>{point.state ? t(`modelMetrics.state.${point.state}`) : '—'}</td></tr>)}</tbody></table></div></details></CardContent></Card>;
}

export function MetricsDashboard({ api, identity, allowed, ledger, usage, settingsPath }: { api: MetricsApi; identity: string; allowed: boolean; ledger?: ReactNode; usage?: ReactNode; settingsPath?: string }) {
    const { t, i18n } = useTranslation();
    const client = useQueryClient();
    const [mountIdentity] = useState(() => globalThis.crypto?.randomUUID?.() ?? `${Date.now()}-${Math.random()}`);
    const [range, setRange] = useState<UsageRangeParams>(() => presetRange('24h', new Date()));
    const [filters, setFilters] = useState<Query>({ granularity: 'hour', limit: 50 });
    const [tab, setTab] = useState('overview');
    const [refresh, setRefresh] = useState(true);
    const [visible, setVisible] = useState(!document.hidden);
    const [selected, setSelected] = useState<string | null>(null);
    const [cursor, setCursor] = useState<string | undefined>();
    const [gapCursor, setGapCursor] = useState<string | undefined>();
    const [invalidAccess, setInvalidAccess] = useState(false);
    const [filtersOpen, setFiltersOpen] = useState(false);
    const [comparisonOpen, setComparisonOpen] = useState(false);
    const [callConditions, setCallConditions] = useState<CallConditions>({});
    const [groupSort, setGroupSort] = useState<NonNullable<Query['group_sort']>>('calls');
    const query = useMemo(() => ({ ...filters, from: range.from, to: range.to }), [filters, range]);
    const effectiveGroupSort = (tab === 'tools' && groupSort.startsWith('request_')) || (groupSort === 'request_failure_rate' && filters.error && !filters.error.startsWith('request.')) || (groupSort === 'input_rejection_rate' && filters.error && !filters.error.startsWith('input.')) ? 'calls' : groupSort;
    const groupQuery = { ...query, group_sort: effectiveGroupSort };
    const callsQuery = useMemo(() => ({ ...query, ...callConditions }), [query, callConditions]);
    const key = useMemo(() => ['model-metrics', location.origin, identity, mountIdentity] as const, [identity, mountIdentity]);
    useEffect(() => { const change = () => setVisible(!document.hidden); document.addEventListener('visibilitychange', change); return () => document.removeEventListener('visibilitychange', change); }, []);
    useEffect(() => { setSelected(null); setCursor(undefined); setGapCursor(undefined); }, [query, callConditions, identity]);
    useEffect(() => { setInvalidAccess(false); return () => { void client.cancelQueries({ queryKey: key }); client.removeQueries({ queryKey: key }); }; }, [client, key]);
    const enabled = allowed && !invalidAccess;
    const rangeVisible = !['ledger', 'usage'].includes(tab);
    const filtersVisible = rangeVisible && tab !== 'unassociated';
    const modelsApplicable = filters.tool == null;
    const callKind = callConditions.record_kind ?? (filters.tool || filters.error?.startsWith('input.') ? 'tool' : 'call');
    const callsApplicable = (!filters.tool || ['tool', 'operation'].includes(callKind)) && (!filters.error || (filters.error.startsWith('input.') ? callKind === 'tool' : filters.error.startsWith('request.') ? ['call', 'attempt'].includes(callKind) : callKind === 'call'));
    const drill = (conditions: CallConditions, scope = filters) => { setFilters({ ...scope, error: undefined }); setCallConditions(conditions); setCursor(undefined); setTab('calls'); };
    const options = { enabled, retry: false, gcTime: 0, refetchInterval: refresh && visible ? 15_000 : false as const };
    const status = useQuery({ queryKey: [...key, 'status'], queryFn: ({ signal }) => api.status(signal), ...options });
    const overview = useQuery({ queryKey: [...key, 'overview', query], queryFn: ({ signal }) => api.overview(query, signal), ...options, enabled: enabled && tab === 'overview' });
    const series = useQuery({ queryKey: [...key, 'series', query], queryFn: ({ signal }) => api.series(query, signal), ...options, enabled: enabled && tab === 'overview' });
    const models = useQuery({ queryKey: [...key, 'models', groupQuery], queryFn: ({ signal }) => api.models(groupQuery, signal), ...options, enabled: enabled && tab === 'models' && modelsApplicable });
    const tools = useQuery({ queryKey: [...key, 'tools', groupQuery], queryFn: ({ signal }) => api.tools(groupQuery, signal), ...options, enabled: enabled && tab === 'tools' });
    const calls = useQuery({ queryKey: [...key, 'calls', callsQuery, cursor], queryFn: ({ signal }) => api.calls({ ...callsQuery, cursor }, signal), ...options, enabled: enabled && tab === 'calls' && callsApplicable });
    const unassociated = useQuery({ queryKey: [...key, 'unassociated', range, gapCursor], queryFn: ({ signal }) => api.unassociated({ from: range.from, to: range.to, cursor: gapCursor, limit: 50 }, signal), ...options, enabled: enabled && tab === 'unassociated' });
    const detail = useQuery({ queryKey: [...key, 'detail', selected], queryFn: ({ signal }) => api.detail(selected!, signal), ...options, enabled: enabled && selected != null });
    const choicesQuery = { ...range, granularity: filters.granularity, include_probe: filters.include_probe, limit: 200 };
    const modelChoices = useQuery({ queryKey: [...key, 'model-choices', choicesQuery], queryFn: ({ signal }) => api.models(choicesQuery, signal), ...options, enabled: enabled && filtersOpen && filtersVisible, refetchInterval: false });
    const toolChoices = useQuery({ queryKey: [...key, 'tool-choices', choicesQuery], queryFn: ({ signal }) => api.tools(choicesQuery, signal), ...options, enabled: enabled && filtersOpen && filtersVisible, refetchInterval: false });
    const accessFailed = [status, overview, series, models, tools, calls, unassociated, detail, modelChoices, toolChoices].some((result) => api.accessError(result.error));
    useEffect(() => { if (accessFailed) { setInvalidAccess(true); setSelected(null); void client.cancelQueries({ queryKey: key }); client.removeQueries({ queryKey: key }); } }, [accessFailed, client, key]);
    if (!allowed || invalidAccess || accessFailed) return <p role="alert" className="p-6">{t('modelMetrics.noAccess')}</p>;
    const active = tab === 'models' ? models : tab === 'tools' ? tools : tab === 'calls' ? calls : tab === 'unassociated' ? unassociated : overview;
    const grouped = tab === 'models' && modelsApplicable ? models.data : tab === 'tools' ? tools.data : undefined;
    return <main className="p-4 md:p-6 space-y-6 min-w-0 overflow-auto">
        <div className="flex flex-wrap justify-between gap-3">
            <div><h1 className="text-2xl font-semibold">{t('modelMetrics.title')}</h1><p className="text-muted-foreground">{t('modelMetrics.description')}</p></div>
            {settingsPath && <Button variant="outline" asChild><Link to={settingsPath}>{t('modelMetrics.settingsTitle')}</Link></Button>}
            {rangeVisible && <div className="flex flex-wrap gap-2 items-center max-w-full">
                <Label className="flex shrink-0 gap-2 whitespace-nowrap"><input type="checkbox" checked={refresh} onChange={(event) => setRefresh(event.target.checked)}/>{t('modelMetrics.refresh')}</Label>
                <Button variant="outline" onClick={() => { void client.invalidateQueries({ queryKey: key }); if (tab === 'runtime') void client.invalidateQueries({ predicate: (query) => query.queryKey[0] === 'model-runtime' && query.queryKey[1] === location.origin && query.queryKey[2] === identity }); }}>{t('modelMetrics.refreshNow')}</Button>
                {tab === 'calls' ? <Button variant="outline" disabled={!callsApplicable || !calls.data || !!calls.error || calls.isFetching} onClick={() => calls.data && downloadMetricsCsv(callsCsv(calls.data, callsQuery), 'model-metrics-calls.csv')}>{t('modelMetrics.exportCalls')}</Button>
                    : ['overview', 'models', 'tools'].includes(tab) && <Button variant="outline" disabled={!!active.error || active.isFetching || (tab === 'overview' ? !overview.data : !grouped)} onClick={() => {
                        const value = tab === 'overview' ? overview.data : grouped;
                        if (value) downloadMetricsCsv(aggregateCsv(value, tab === 'overview' ? query : groupQuery), 'model-metrics.csv');
                    }}>{t('modelMetrics.export')}</Button>}
            </div>}
        </div>
        <p className="text-xs text-muted-foreground">{t('modelMetrics.localTimeHint')}</p>
        <Card><CardContent className="pt-5 space-y-2">
            <p role="status">{t(`modelMetrics.state.${status.isLoading ? 'initializing' : status.error ? 'unavailable' : status.data?.state ?? 'unavailable'}`)} · {t('modelMetrics.asOf')}: <MetricTime value={status.data?.as_of}/></p>
            <p className="text-sm text-muted-foreground">{t('modelMetrics.availableFrom')}: <MetricTime value={status.data?.available_from}/> · {t('modelMetrics.backlog')}: {decimalText(status.data?.backlog)} · {t('modelMetrics.dropped')}: {decimalText(status.data?.dropped_events)}</p>
            {status.data?.gaps.map((gap) => <p key={`${gap.from}-${gap.reason}`} className="text-sm"><MetricTime value={gap.from}/> — <MetricTime value={gap.to}/>: {t(`modelMetrics.gap.${gap.reason}`, { defaultValue: gap.reason })}</p>)}
            <p className="text-sm text-muted-foreground">{t('modelMetrics.coverageHint')}</p>
            {status.data && <details className="text-sm"><summary>{t('modelMetrics.healthDetails')}</summary><dl className="grid gap-2 pt-2 sm:grid-cols-2">
                {([['lastPersisted', metricTime(status.data.last_persisted, i18n.language)], ['lastAggregated', metricTime(status.data.last_aggregated, i18n.language)], ['oldestPending', metricTime(status.data.oldest_pending, i18n.language)], ['discarded', decimalText(status.data.discarded_events)], ['unassociatedCount', decimalText(status.data.unassociated_records)], ['settingsRevision', status.data.settings_revision]] as const).map(([name, value]) => <div key={name}><dt className="text-muted-foreground">{t(`modelMetrics.${name}`)}</dt><dd>{value ?? '—'}</dd></div>)}
                <div><dt className="text-muted-foreground">{t('modelMetrics.settingsEffective')}</dt><dd>{t(status.data.settings_effective ? 'modelMetrics.value.yes' : 'modelMetrics.value.no')}</dd></div>
                <div><dt className="text-muted-foreground">{t('modelMetrics.instrumentedSurfaces')}</dt><dd>{status.data.instrumented_surfaces.map((surface) => t(`modelMetrics.value.surface.${surface}`, { defaultValue: surface })).join(', ') || '—'}</dd></div>
                <div><dt className="text-muted-foreground">{t('modelMetrics.unsupportedSurfaces')}</dt><dd>{status.data.unsupported_surfaces.map((surface) => t(`modelMetrics.value.surface.${surface}`, { defaultValue: surface })).join(', ') || '—'}</dd></div>
            </dl></details>}
            {status.data?.storage && <Storage value={status.data.storage}/>}
        </CardContent></Card>
        {rangeVisible && <UsageRangePicker value={range} onChange={setRange}/>}
        {filtersVisible && <details onToggle={(event) => setFiltersOpen(event.currentTarget.open)}><summary>{t('modelMetrics.filters')}</summary>
            <MetricsFilters value={filters} change={setFilters} models={modelChoices.data} tools={toolChoices.data} loading={modelChoices.isLoading || toolChoices.isLoading} failed={!!modelChoices.error || !!toolChoices.error}/>
        </details>}
        {(tab === 'ledger' || tab === 'usage') && <p className="text-sm text-muted-foreground">{t(tab === 'ledger' ? 'modelMetrics.ledgerFiltersHint' : 'modelMetrics.usageFiltersHint')}</p>}
        <div role="tablist" aria-label={t('modelMetrics.title')} className="flex gap-2 overflow-auto">
            {['overview', 'models', 'tools', 'calls', 'runtime', 'unassociated', ...(ledger ? ['ledger'] : []), ...(usage ? ['usage'] : [])].map((name) => <Button key={name} role="tab" aria-selected={tab === name} variant={tab === name ? 'default' : 'outline'} onClick={() => setTab(name)}>{t(`modelMetrics.tab.${name}`)}</Button>)}
        </div>
        {tab === 'runtime' ? <MetricsRuntimePanel api={api} identity={identity} allowed={enabled} queryFilters={query} autoRefresh={refresh}/>
            : tab === 'usage' ? usage : tab === 'ledger' ? ledger : <section role="tabpanel" className="space-y-5">
                {active.isLoading && !(tab === 'models' && !modelsApplicable) && !(tab === 'calls' && !callsApplicable) && <p>{t('modelMetrics.loading')}</p>}
                {active.error && <p role="alert">{t('modelMetrics.queryFailed')}</p>}
                {tab === 'overview' && overview.data && <>
                    <MetricsCoverage value={overview.data.coverage}/>
                    {overview.data.coverage.sample_status !== 'not_collected' && overview.data.coverage.sample_status !== 'unavailable' && <Summary value={overview.data.summary} previous={overview.data.previous} drill={drill} tool={filters.tool}/>}
                    {series.error && <p role="alert">{t('modelMetrics.trendUnavailable')}</p>}
                    {series.data && <Trend value={series.data}/>}
                    <details open={comparisonOpen} onToggle={(event) => setComparisonOpen(event.currentTarget.open)}><summary>{t('modelMetrics.compareWindows')}</summary>{comparisonOpen && <MetricsComparison key={JSON.stringify(query)} api={api} query={query} cacheKey={key} accessLost={() => setInvalidAccess(true)}/>}</details>
                </>}
                {tab === 'models' && !modelsApplicable && <div role="status" className="space-y-3"><p>{t('modelMetrics.modelToolNotApplicable')}</p><Button variant="outline" onClick={() => setFilters({ ...filters, tool: undefined })}>{t('modelMetrics.clearToolFilter')}</Button></div>}
                {(tab === 'models' || tab === 'tools') && <Label className="flex flex-wrap items-center gap-3">{t('modelMetrics.groupSort')}<select aria-label={t('modelMetrics.groupSort')} className="h-9 rounded border bg-background px-2" value={effectiveGroupSort} onChange={(event) => setGroupSort(event.target.value as NonNullable<Query['group_sort']>)}>{['calls', ...(tab === 'models' ? ['request_errors', ...(!filters.error || filters.error.startsWith('request.') ? ['request_failure_rate'] : [])] : []), 'input_rejected', ...(!filters.error || filters.error.startsWith('input.') ? ['input_rejection_rate'] : [])].map((sort) => <option key={sort} value={sort}>{t(`modelMetrics.sort.${sort}`)}</option>)}</select></Label>}
                {grouped && <MetricsCoverage value={grouped.coverage} empty={grouped.groups.length === 0 && !grouped.other}/>}
                {grouped?.groups.map((group) => <Card key={group.key}><CardHeader>
                    <CardTitle className="flex flex-wrap justify-between gap-3 text-base">
                        {group.tool ?? group.model_name ?? t('modelMetrics.value.unknown')}
                        <Button size="sm" variant="outline" disabled={tab === 'models' && !group.model_id} onClick={() => {
                            setFilters(tab === 'tools' ? { ...filters, tool: group.tool ?? undefined } : { ...filters, model_id: group.model_id ?? undefined, provider_id: group.provider_id ?? undefined });
                            setCallConditions({}); setCursor(undefined); setTab('calls');
                        }}>{t('modelMetrics.drillDown')}</Button>
                    </CardTitle>
                    {tab === 'models' && <CardDescription>{t('modelMetrics.filter.provider_id')}: {group.provider_id ?? '—'} · {t('modelMetrics.filter.model_id')}: {group.model_id ?? '—'}</CardDescription>}
                    {tab === 'models' && !group.model_id && <CardDescription>{t('modelMetrics.unresolvedModelHint')}</CardDescription>}
                </CardHeader><CardContent className="space-y-5"><Summary value={group.summary} tool={group.tool ?? filters.tool} drill={tab === 'models' && !group.model_id ? undefined : (conditions) => drill(conditions, tab === 'tools' ? { ...filters, tool: group.tool ?? undefined } : { ...filters, provider_id: group.provider_id ?? undefined, model_id: group.model_id ?? undefined })}/>{tab === 'tools' && <div className="space-y-2"><h3 className="font-medium">{t('modelMetrics.associatedModels')}</h3>{group.associated_models.map((model) => <div key={`${model.provider_id}:${model.model_id}`} className="flex flex-wrap items-center justify-between gap-2 text-sm"><p>{model.model_name} · {model.provider_id} / {model.model_id} · {t('modelMetrics.count.tools')}: {decimalText(model.tool_inputs)}</p><Button size="sm" variant="outline" onClick={() => { setFilters({ ...filters, provider_id: model.provider_id, model_id: model.model_id, tool: group.tool ?? undefined }); setCallConditions({}); setCursor(undefined); setTab('calls'); }}>{t('modelMetrics.drillDown')}</Button></div>)}{group.other_model_count !== '0' && <p className="text-sm text-muted-foreground">{t('modelMetrics.otherModels', { countText: decimalText(group.other_model_count) })}</p>}</div>}</CardContent></Card>)}
                {grouped?.other && <Card><CardHeader><CardTitle>{t('modelMetrics.other')}</CardTitle><CardDescription>{t('modelMetrics.otherHint')}</CardDescription></CardHeader><CardContent><Summary value={grouped.other}/></CardContent></Card>}
                {tab === 'calls' && <CallFilters value={callConditions} change={(value) => { setCallConditions(value); setCursor(undefined); }} inferredKind={filters.tool || filters.error?.startsWith('input.') ? 'tool' : 'call'}/>}
                {tab === 'calls' && !callsApplicable && <p role="status">{t('modelMetrics.callsToolNotApplicable')}</p>}
                {tab === 'calls' && callsApplicable && calls.data && <>
                    <MetricsCoverage value={calls.data.coverage} empty={calls.data.records.length === 0}/>
                    <div className="overflow-x-auto"><table className="w-full text-sm"><thead><tr className="text-left border-b">
                        <th>{t('modelMetrics.time')}</th><th>{t('modelMetrics.model')}</th><th>{t('modelMetrics.field.kind')}</th><th>{t('modelMetrics.filter.purpose')}</th><th>{t('modelMetrics.filter.origin')}</th><th>{t('modelMetrics.outcome')}</th><th>{t('modelMetrics.duration')}</th><th>{t('modelMetrics.callInputCounts')}</th><th>{t('modelMetrics.callUsage')}</th><th>{t('modelMetrics.detail')}</th>
                    </tr></thead><tbody>{calls.data.records.map((row) => <tr key={row.id} className="border-b">
                        <td className="py-3"><MetricTime value={row.started_at}/></td><td>{row.model_name || t('modelMetrics.value.unknown')}<p className="text-xs text-muted-foreground break-all">{row.provider_id || t('modelMetrics.value.unknown')} / {row.model_id || t('modelMetrics.value.unknown')}</p><p className="text-xs text-muted-foreground">{row.tool ?? ''}</p></td>
                        <td>{t(`modelMetrics.value.kind.${row.kind}`)}</td><td>{t(`modelMetrics.value.purpose.${row.purpose}`)}</td><td>{t(`modelMetrics.value.origin.${row.origin}`)}</td>
                        <td><RecordOutcome record={row}/>{row.input_issue && row.input_issue !== 'none' && <p className="text-xs text-muted-foreground">{t(`modelMetrics.value.issue.${row.input_issue}`)}</p>}</td>
                        <td>{row.duration_ms == null ? t('modelMetrics.value.unknown') : t('modelMetrics.milliseconds', { value: row.duration_ms })}</td>
                        <td className="min-w-36">{row.kind === 'call' ? <><p className="font-mono">{decimalText(row.tool_count)} / {decimalText(row.input_rejected_count)}</p><p className="text-xs text-muted-foreground">{t(`modelMetrics.state.${row.tool_counts_status}`)}</p>{row.generated_tool_count != null && <p className="text-xs">{t('modelMetrics.generatedInputs', { countText: decimalText(row.generated_tool_count) })}</p>}</> : t('modelMetrics.state.not_applicable')}</td>
                        <td className="min-w-36">{row.kind === 'call' ? <><p className="font-mono">{row.input_tokens == null ? t('modelMetrics.value.unknown') : decimalText(row.input_tokens)} / {row.output_tokens == null ? t('modelMetrics.value.unknown') : decimalText(row.output_tokens)}</p><p className="text-xs text-muted-foreground">{t(row.usage_complete ? 'modelMetrics.state.complete' : 'modelMetrics.state.partial')}</p></> : t('modelMetrics.state.not_applicable')}</td><td><Button variant="ghost" size="sm" onClick={() => setSelected(row.id)}>{t('modelMetrics.detail')}</Button></td>
                    </tr>)}</tbody></table></div>
                    <div className="flex gap-3"><Button variant="outline" disabled={!cursor} onClick={() => setCursor(undefined)}>{t('modelMetrics.firstPage')}</Button><Button variant="outline" disabled={!calls.data.next_cursor} onClick={() => setCursor(calls.data.next_cursor ?? undefined)}>{t('modelMetrics.nextPage')}</Button></div>
                    <p className="text-sm text-muted-foreground">{t('modelMetrics.livePagination')}</p>
                    <p className="text-sm text-muted-foreground">{t('modelMetrics.exportCallsHint')}</p>
                    <p className="text-sm text-muted-foreground">{t('modelMetrics.callCountsHint')}</p>
                </>}
                {tab === 'unassociated' && unassociated.data && !unassociated.error && <>
                    <UnassociatedFacts value={unassociated.data} select={setSelected}/>
                    <div className="flex gap-3"><Button variant="outline" disabled={!gapCursor} onClick={() => setGapCursor(undefined)}>{t('modelMetrics.firstPage')}</Button><Button variant="outline" disabled={!unassociated.data.next_cursor} onClick={() => setGapCursor(unassociated.data.next_cursor ?? undefined)}>{t('modelMetrics.nextPage')}</Button></div>
                    <p className="text-sm text-muted-foreground">{t('modelMetrics.livePagination')}</p>
                </>}

            </section>}
        <Dialog open={selected != null} onOpenChange={(open) => { if (!open) setSelected(null); }}>
            <DialogContent className="max-w-4xl max-h-[90vh] overflow-auto"><DialogHeader><DialogTitle>{t('modelMetrics.detail')}</DialogTitle><DialogDescription>{t('modelMetrics.detailHint')}</DialogDescription></DialogHeader>
                {detail.isLoading && <p>{t('modelMetrics.loading')}</p>}{detail.error && <p role="alert">{t('modelMetrics.queryFailed')}</p>}
                {detail.data && !detail.error && <>
                    {[detail.data.call, ...detail.data.related].map((record) => <ObservationDetails key={record.id} record={record} expanded={record.id === selected} select={setSelected}/>)}
                    {detail.data.related_truncated && <p>{t('modelMetrics.relatedTruncated')}</p>}
                </>}
            </DialogContent>
        </Dialog>
    </main>;
}
