import { useEffect, useState } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import type { ApprovalMetricsOverviewDto, GetApprovalMetricsOverviewQueryParams, MetricsGroups, LatencySummary } from '@/services/types';
import type { UsageRangeParams } from '@/features/usage/usage-range-picker';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { SelectField } from '@/components/ui/select-field';
import { SelectItem } from '@/components/ui/select';
import { Label } from '@/components/ui/label';
import { MetricsCoverage } from '@/features/model-metrics/coverage';
import { decimalText } from '@/features/model-metrics/format';
import { approvalAccessError } from './api';

type Loader = (query: GetApprovalMetricsOverviewQueryParams, signal: AbortSignal) => Promise<ApprovalMetricsOverviewDto>;
export function ApprovalMetricsPanel({ load, range, identity, enabled, autoRefresh, models, manager = false }: { load: Loader; range: UsageRangeParams; identity: string; enabled: boolean; autoRefresh: boolean; models?: MetricsGroups; manager?: boolean }) {
    const { t } = useTranslation();
    const client = useQueryClient();
    const [filters, setFilters] = useState<GetApprovalMetricsOverviewQueryParams>({ granularity: 'hour' });
    const [blocked, setBlocked] = useState(false);
    const key = ['approval-platform', location.origin, identity];
    const params = { ...filters, ...range };
    const query = useQuery({ queryKey: [...key, params], queryFn: ({ signal }) => load(params, signal), enabled: enabled && !blocked, retry: false, gcTime: 0, refetchInterval: autoRefresh && !document.hidden ? 15000 : false });
    const accessFailed = approvalAccessError(query.error);
    useEffect(() => { setBlocked(false); return () => { void client.cancelQueries({ queryKey: key }); client.removeQueries({ queryKey: key }); }; }, [client, identity]);
    useEffect(() => { if (accessFailed) { setBlocked(true); void client.cancelQueries({ queryKey: key }); client.removeQueries({ queryKey: key }); } }, [accessFailed, client, identity]);
    if (!enabled || accessFailed || blocked) return <p role="alert">{t('approval.noAccess')}</p>;
    const value = query.error ? undefined : query.data;
    const percentage = (number?: number | null) => number == null ? '—' : `${(number * 100).toFixed(2)}%`;
    function latency(summary: LatencySummary) {
        return <dl className="grid gap-2 text-sm sm:grid-cols-4">
            <div><dt>{t('approval.samples')}</dt><dd>{decimalText(summary.count)}</dd></div>
            {(['average_ms', 'p50_ms', 'p95_ms', 'p99_ms'] as const).map(key => <div key={key}><dt>{t(`approval.latency.${key}`)}</dt><dd>{summary[key] == null ? '—' : summary[key]! > Number.MAX_SAFE_INTEGER ? t('approval.overflow') : `${Math.round(summary[key]!)} ${t('approval.milliseconds')}`}</dd></div>)}
            {summary.overflow_count !== '0' && <p className="text-muted-foreground">{t('approval.overflowSamples', { count: decimalText(summary.overflow_count) })}</p>}
        </dl>;
    }
    return <section role="tabpanel" className="space-y-4">
        <p className="text-sm text-muted-foreground">{t('approval.platformHint')}</p>
        <Label className="grid max-w-xs gap-2">{t('approval.granularity')}<SelectField value={filters.granularity ?? 'hour'} onValueChange={value => setFilters({ ...filters, granularity: value === 'five_minutes' ? 'five_minutes' : 'hour' })}>{['hour', 'five_minutes'].map(value => <SelectItem key={value} value={value}>{t(`approval.interval.${value}`)}</SelectItem>)}</SelectField></Label>
        <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
            <Label className="grid gap-2">{t('approval.model')}<SelectField value={filters.model_id ? JSON.stringify([filters.provider_id, filters.model_id]) : ''} onValueChange={selected => {
                const pair = selected ? JSON.parse(selected) as [string, string] : undefined;
                setFilters({ ...filters, provider_id: pair?.[0], model_id: pair?.[1] });
            }}><SelectItem value="__empty__">{t('modelMetrics.all')}</SelectItem>{models?.groups.filter(group => group.model_id).map(group => <SelectItem key={group.key} value={JSON.stringify([group.provider_id, group.model_id])}>{group.model_name}</SelectItem>)}</SelectField></Label>
            {([
                ['source_kind', manager ? ['permission_item', 'concrete_call', 'manager_command'] : ['permission_item', 'concrete_call']],
                ['session_kind', ['main', 'subagent', 'unknown']],
                ['reason', ['configuration', 'provider', 'transport', 'timeout', 'stream', 'output_truncated', 'invalid_decision', 'policy', 'budget', 'revoked', 'source_changed', 'candidate_expired', 'lease_expired', 'processing', 'unknown']],
                ['outcome', ['pending', 'approved', 'denied', 'fault', 'policy_blocked', 'cancelled', 'superseded', 'expired', 'unknown']],
            ] as const).map(([key, choices]) => <Label key={key} className="grid gap-2">{t(key === 'source_kind' ? 'approval.source' : key === 'session_kind' ? 'approval.sessionKind' : key === 'reason' ? 'approval.failureCategory' : 'approval.outcome')}<SelectField value={filters[key] ?? ''} onValueChange={selected => setFilters({ ...filters, [key]: selected || undefined })}><SelectItem value="__empty__">{t('modelMetrics.all')}</SelectItem>{choices.map(choice => <SelectItem key={choice} value={choice}>{t(`approval.${key === 'source_kind' ? 'source' : key === 'session_kind' ? 'sessionKind' : key === 'reason' ? 'reason' : 'outcome'}.${choice}`)}</SelectItem>)}</SelectField></Label>)}
        </div>
        {query.isLoading && <p>{t('common.loading')}</p>}
        {query.error && <p role="alert">{t('approval.failed')}</p>}
        {value && <>
            <MetricsCoverage value={value.coverage}/><p className="text-xs text-muted-foreground">{t('approval.updated')}: {new Date(value.as_of_ms).toLocaleString()}</p>
            <p className="text-sm">{t('approval.rateHint', { count: decimalText(value.rates.denominator) })}</p>
            <div className="grid gap-3 sm:grid-cols-3">{[['approvalRate', value.rates.approval_rate, value.rates.approved], ['denialRate', value.rates.denial_rate, value.rates.denied], ['faultRate', value.rates.fault_rate, value.rates.fault]] .map(([key, number, count]) => <Card key={String(key)}><CardHeader><CardTitle className="text-sm">{t(`approval.${key}`)}</CardTitle></CardHeader><CardContent><p className="text-2xl font-semibold">{percentage(number as number | null)}</p><p>{decimalText(String(count))} / {decimalText(value.rates.denominator)}</p></CardContent></Card>)}</div>
            {value.rates.denominator === '0' && <p>{t('approval.noSamples')}</p>}
            <div className="flex flex-wrap gap-3 text-sm">{value.counts.map(item => <span key={item.kind}>{t(`approval.outcome.${item.kind}`)}: {decimalText(item.count)}</span>)}</div>
            <Card><CardHeader><CardTitle>{t('approval.waiting')}</CardTitle><CardDescription>{t('approval.waitingHint')}</CardDescription></CardHeader><CardContent>{latency(value.waiting)}<p className="mt-2 text-xs text-muted-foreground">{t('approval.unknownWaiting', { count: decimalText(value.unknown_waiting) })}</p></CardContent></Card>
            <Card><CardHeader><CardTitle>{t('approval.beforeModel')}</CardTitle></CardHeader><CardContent>{latency(value.before_model)}</CardContent></Card>
            <Card><CardHeader><CardTitle>{t('approval.pendingAge')}</CardTitle></CardHeader><CardContent>{latency(value.pending_age)}</CardContent></Card>
            <Card><CardHeader><CardTitle>{t('approval.distribution')}</CardTitle><CardDescription>{t('approval.distributionHint')}</CardDescription></CardHeader><CardContent className="grid gap-4 sm:grid-cols-3">{value.session_kinds.map(group => <div key={group.key}><h3 className="font-medium">{t(`approval.sessionKind.${group.key}`)}</h3><p>{decimalText(group.count)} · {percentage(group.share)}</p><p className="text-xs">{t('approval.approvalRate')}: {percentage(group.rates.approval_rate)} · {t('approval.denialRate')}: {percentage(group.rates.denial_rate)} · {t('approval.faultRate')}: {percentage(group.rates.fault_rate)}</p>{latency(group.waiting)}</div>)}</CardContent></Card>
            <Card><CardHeader><CardTitle>{t('approval.trend')}</CardTitle></CardHeader><CardContent className="max-h-80 space-y-2 overflow-y-auto text-xs">{value.timeseries.map(point => <div key={point.bucket} className="grid grid-cols-2 gap-2 sm:grid-cols-5"><span>{new Date(point.bucket).toLocaleString()}</span><span>{decimalText(point.count)}</span><span>{t('approval.approvalRate')}: {percentage(point.rates.approval_rate)}</span><span>{t('approval.denialRate')}: {percentage(point.rates.denial_rate)}</span><span>{t('approval.faultRate')}: {percentage(point.rates.fault_rate)}</span></div>)}</CardContent></Card>
        </>}
    </section>;
}
