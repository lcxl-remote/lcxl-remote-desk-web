import { useEffect, useState } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import type { GetModelMetricsOverviewQueryParams, MetricsGroups, MetricsOverview } from '@/services/types';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Label } from '@/components/ui/label';
import { Input } from '@/components/ui/input';
import { UsageRangePicker, type UsageRangeParams } from '@/features/usage/usage-range-picker';
import { MetricsCoverage } from './coverage';
import { decimalText } from './format';
import type { MetricsApi } from './dashboard';

type Query = GetModelMetricsOverviewQueryParams;
type Window = { range: UsageRangeParams; revision: string };

function previousRange(query: Query): UsageRangeParams {
    const to = new Date(query.to!).getTime();
    const from = new Date(query.from!).getTime();
    if (!Number.isFinite(to) || !Number.isFinite(from) || to <= from) return {};
    return { from: new Date(Math.max(0, from - (to - from))).toISOString(), to: query.from ?? undefined };
}

function revisions(groups?: MetricsGroups): Map<string, Set<string>> {
    const result = new Map<string, Set<string>>();
    for (const group of groups?.groups ?? []) for (const model of group.configurations) {
        const key = `${model.provider_id} / ${model.model_id}`;
        const values = result.get(key) ?? new Set<string>();
        model.revisions.forEach((revision) => values.add(revision)); result.set(key, values);
    }
    return result;
}

export function MetricsComparison({ api, query, cacheKey, accessLost }: { api: MetricsApi; query: Query; cacheKey: readonly unknown[]; accessLost: () => void }) {
    const { t } = useTranslation();
    const client = useQueryClient();
    const [windows, setWindows] = useState<[Window, Window]>(() => [
        { range: previousRange(query), revision: query.contract_revision ?? '' },
        { range: { from: query.from ?? undefined, to: query.to ?? undefined }, revision: query.contract_revision ?? '' },
    ]);
    const queries = windows.map((window) => ({ ...query, ...window.range, contract_revision: window.revision || undefined }));
    const valid = windows.map((window) => {
        const from = Date.parse(window.range.from ?? ''); const to = Date.parse(window.range.to ?? '');
        return Number.isFinite(from) && Number.isFinite(to) && from >= 0 && to > from && to - from <= 90 * 86400000;
    });
    const options = { retry: false, gcTime: 0, refetchInterval: false as const };
    const a = useQuery({ queryKey: [...cacheKey, 'comparison', 'a', queries[0]], queryFn: ({ signal }) => api.overview(queries[0], signal), ...options, enabled: valid[0] });
    const b = useQuery({ queryKey: [...cacheKey, 'comparison', 'b', queries[1]], queryFn: ({ signal }) => api.overview(queries[1], signal), ...options, enabled: valid[1] });
    const configurationQuery = (index: number) => ({ ...queries[index], limit: 200 });
    const am = useQuery({ queryKey: [...cacheKey, 'comparison-configurations', configurationQuery(0)], queryFn: ({ signal }) => (query.tool ? api.tools : api.models)(configurationQuery(0), signal), ...options, enabled: valid[0] });
    const bm = useQuery({ queryKey: [...cacheKey, 'comparison-configurations', configurationQuery(1)], queryFn: ({ signal }) => (query.tool ? api.tools : api.models)(configurationQuery(1), signal), ...options, enabled: valid[1] });
    const denied = [a, b, am, bm].some((result) => api.accessError(result.error));
    useEffect(() => { if (denied) { void client.cancelQueries({ queryKey: cacheKey }); client.removeQueries({ queryKey: cacheKey }); accessLost(); } }, [denied, client, cacheKey, accessLost]);
    if (denied) return null;
    const sources = [a, b];
    const configs = [revisions(am.data), revisions(bm.data)];
    const configModels = [...new Set([...configs[0].keys(), ...configs[1].keys()])].sort();
    const shown = (overview: MetricsOverview | undefined, key: string) => {
        const rate = overview?.summary.rates.find((rate) => rate.key === key);
        return <>{rate?.value == null ? '—' : `${(rate.value * 100).toFixed(2)}%`}<p className="font-mono text-xs">{decimalText(rate?.numerator)} / {decimalText(rate?.denominator)}</p>{rate && <p className="text-xs">{t(`modelMetrics.state.${rate.sample_status}`)}</p>}</>;
    };
    return <Card><CardHeader><CardTitle>{t('modelMetrics.compareWindows')}</CardTitle></CardHeader><CardContent className="space-y-5">
        <p className="text-sm text-muted-foreground">{t('modelMetrics.comparisonHint')}</p>
        <div className="grid gap-5 lg:grid-cols-2">{windows.map((window, index) => <section key={index} className="space-y-3" aria-label={t(index === 0 ? 'modelMetrics.windowA' : 'modelMetrics.windowB')}>
            <h3 className="font-semibold">{t(index === 0 ? 'modelMetrics.windowA' : 'modelMetrics.windowB')}</h3>
            <UsageRangePicker value={window.range} onChange={(range) => setWindows((old) => old.map((item, position) => position === index ? { ...item, range } : item) as [Window, Window])}/>
            <Label className="space-y-2">{t('modelMetrics.filter.contract_revision')}<Input aria-label={`${index === 0 ? 'A' : 'B'} ${t('modelMetrics.filter.contract_revision')}`} maxLength={128} value={window.revision} onChange={(event) => setWindows((old) => old.map((item, position) => position === index ? { ...item, revision: event.target.value } : item) as [Window, Window])}/></Label>
            {!valid[index] && <p role="alert">{t('modelMetrics.comparisonInvalidRange')}</p>}{sources[index].isLoading && <p>{t('modelMetrics.loading')}</p>}{sources[index].error && <p role="alert">{t('modelMetrics.queryFailed')}</p>}
            {sources[index].data && !sources[index].error && <MetricsCoverage value={sources[index].data!.coverage}/>}
        </section>)}</div>
        <div className="overflow-x-auto"><table className="w-full text-sm"><thead><tr className="text-left"><th>{t('modelMetrics.metric')}</th><th>A</th><th>B</th></tr></thead><tbody>{(a.data?.summary.rates ?? b.data?.summary.rates ?? []).map((rate) => <tr key={rate.key} className="border-b"><td className="py-3">{t(`modelMetrics.rate.${rate.key}`)}{rate.numerator_error && <p className="text-xs text-muted-foreground">{t('modelMetrics.selectedError', { error: t(`modelMetrics.error.${rate.numerator_error}`) })}</p>}<p className="text-xs text-muted-foreground">{t('modelMetrics.denominatorScope', { scope: t(`modelMetrics.denominator.${rate.denominator_scope}`) })}</p></td><td>{shown(a.error ? undefined : a.data, rate.key)}</td><td>{shown(b.error ? undefined : b.data, rate.key)}</td></tr>)}
            {['calls', 'tools', 'input_rejected'].map((key) => <tr key={key} className="border-b"><td className="py-3">{t(`modelMetrics.count.${key}`)}</td>{sources.map((source, index) => <td key={index} className="font-mono">{source.error || !source.data || ['not_collected', 'unavailable'].includes(source.data.coverage.sample_status) ? t('modelMetrics.value.unknown') : decimalText(source.data.summary.counts.find((count) => count.key === key)?.count ?? '0')}</td>)}</tr>)}
            <tr className="border-b"><td className="py-3">{t('modelMetrics.durationP95')}</td>{sources.map((source, index) => <td key={index} className="font-mono">{source.error || source.data?.summary.duration.p95_ms == null ? t('modelMetrics.value.unknown') : t('modelMetrics.milliseconds', { value: source.data.summary.duration.p95_ms })}</td>)}</tr>
        </tbody></table></div>
        <p className="text-sm">{t('modelMetrics.configurationComparison')}</p>
        {am.error || bm.error ? <p role="status">{t('modelMetrics.configurationUnknown')}</p> : <div className="overflow-x-auto"><table className="w-full text-sm"><thead><tr className="text-left"><th>{t('modelMetrics.model')}</th><th>A</th><th>B</th><th>{t('modelMetrics.state')}</th></tr></thead><tbody>{configModels.map((model) => {
            const ar = [...(configs[0].get(model) ?? [])].sort(); const br = [...(configs[1].get(model) ?? [])].sort();
            const changed = ar.length > 1 || br.length > 1 || JSON.stringify(ar) !== JSON.stringify(br);
            return <tr key={model}><td className="break-all font-mono">{model}</td><td>{ar.join(', ') || t('modelMetrics.value.unknown')}</td><td>{br.join(', ') || t('modelMetrics.value.unknown')}</td><td>{t(!ar.length || !br.length ? 'modelMetrics.configurationUnknown' : changed ? 'modelMetrics.configurationChanged' : 'modelMetrics.configurationStable')}</td></tr>;
        })}</tbody></table></div>}
        {(!configModels.length || [am.data, bm.data].some((groups) => !groups || groups.other || groups.coverage.sample_status !== 'complete' || groups.groups.some((group) => group.configurations_limited))) && <p role="status" className="text-sm text-muted-foreground">{t('modelMetrics.configurationUnknown')}</p>}
    </CardContent></Card>;
}
