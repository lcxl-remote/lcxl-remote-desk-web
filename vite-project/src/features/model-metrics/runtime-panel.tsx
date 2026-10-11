import { Table, TableHeader, TableBody, TableRow, TableHead, TableCell } from '@/components/ui/table';
import { useEffect, useMemo, useState } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { SelectItem } from '@/components/ui/select';
import { SelectField } from '@/components/ui/select-field';
import { UsageRangePicker, presetRange, type UsageRangeParams } from '@/features/usage/usage-range-picker';
import type { MetricsApi } from './dashboard';
import type { MetricsQuery } from './filters';
import { MetricsCoverage } from './coverage';
import { decimalText } from './format';

export function MetricsRuntimePanel({ api, identity, allowed, category, queryFilters, autoRefresh = true }: {
    api: MetricsApi;
    identity: string;
    allowed: boolean;
    category?: string;
    queryFilters?: MetricsQuery;
    autoRefresh?: boolean;
}) {
    const { t } = useTranslation();
    const client = useQueryClient();
    const [epoch] = useState(() => globalThis.crypto?.randomUUID?.() ?? `${Date.now()}-${Math.random()}`);
    const [range, setRange] = useState<UsageRangeParams>(() => presetRange('24h', new Date()));
    const [selected, setSelected] = useState('');
    const [visible, setVisible] = useState(document.visibilityState === 'visible');
    const [invalidAccess, setInvalidAccess] = useState(false);
    const prefix = useMemo(() => ['model-runtime', location.origin, identity, epoch], [identity, epoch]);
    const filters = { ...(queryFilters ?? { ...range, granularity: 'hour' as const }), category: category ?? (selected || undefined), limit: 200 };
    const unsupported = (['provider_id', 'model_id', 'tool', 'error'] as const).filter((field) => filters[field] != null);
    const query = useQuery({
        queryKey: [...prefix, filters],
        queryFn: ({ signal }) => api.runtime(filters, signal),
        enabled: allowed && !invalidAccess && unsupported.length === 0, retry: false, gcTime: 0,
        refetchInterval: autoRefresh && visible ? 15000 : false,
    });
    useEffect(() => {
        const changed = () => setVisible(document.visibilityState === 'visible');
        document.addEventListener('visibilitychange', changed);
        return () => document.removeEventListener('visibilitychange', changed);
    }, []);
    useEffect(() => {
        setInvalidAccess(false);
        return () => { void client.cancelQueries({ queryKey: prefix }); client.removeQueries({ queryKey: prefix }); };
    }, [client, prefix]);
    const accessFailed = api.accessError(query.error);
    useEffect(() => {
        if (!allowed || accessFailed) {
            setInvalidAccess(true);
            void client.cancelQueries({ queryKey: prefix }); client.removeQueries({ queryKey: prefix });
        }
    }, [allowed, accessFailed, client, prefix]);
    if (!allowed || invalidAccess || accessFailed) return <p role="alert">{t('modelMetrics.noAccess')}</p>;
    return <Card><CardHeader><CardTitle>{t('modelMetrics.tab.runtime')}</CardTitle><CardDescription>{t('modelMetrics.runtimeHint')}</CardDescription></CardHeader><CardContent className="space-y-4">
        <div className="flex flex-wrap gap-3 items-center">
            {!queryFilters && <UsageRangePicker value={range} onChange={setRange}/>}
            {!category && <SelectField aria-label={t('modelMetrics.runtimeCategory')} value={selected} onValueChange={setSelected} className="w-full sm:w-56">
                <SelectItem value="__empty__">{t('modelMetrics.all')}</SelectItem>
                {['turn', 'compression', 'inventory', 'projection', 'safety', 'support', 'admission', 'budget', 'estimator', 'fence', 'remote_tool', 'registration', 'audit'].map((value) => <SelectItem value={value} key={value}>{t(`modelMetrics.runtimeCategory.${value}`)}</SelectItem>)}
            </SelectField>}
            <Button variant="outline" disabled={query.isFetching || unsupported.length > 0} onClick={() => void query.refetch()}>{t('modelMetrics.refreshNow')}</Button>
        </div>
        {unsupported.length > 0 ? <p role="status">{t('modelMetrics.runtimeNotApplicable', { filters: unsupported.map((field) => t(`modelMetrics.filter.${field}`)).join(', ') })}</p> : <>
            {query.isLoading && <p>{t('modelMetrics.loading')}</p>}{query.error && <p role="alert">{t('modelMetrics.queryFailed')}</p>}
            {query.data && <>
                <MetricsCoverage value={query.data.coverage} empty={query.data.groups.length === 0 && !query.data.other}/>
                {query.data.groups.map((group, index) => <div className="rounded border p-3 space-y-2" key={`${group.definition}:${index}`}>
                    <p className="font-medium">{t(`modelMetrics.runtimeDefinition.${group.definition}`, { defaultValue: group.definition })}</p>
                    <dl className="flex flex-wrap gap-x-4 gap-y-1 text-xs text-muted-foreground">{Object.entries(group.labels).map(([key, value]) => <div key={key}><dt className="inline">{t(`modelMetrics.runtimeLabel.${key}`, { defaultValue: key })}: </dt><dd className="inline">{t(`modelMetrics.runtimeValue.${value}`, { defaultValue: value })}</dd></div>)}</dl>
                    <p className="text-sm font-mono">{t('modelMetrics.eventCount')}: {decimalText(group.summary.counts.find((value) => value.key === 'runtime_events')?.count)} · {t('modelMetrics.quantitySum')}: {decimalText(group.summary.counts.find((value) => value.key === 'runtime_value')?.count)}</p>
                    {group.summary.other_duration.find((value) => value.kind === 'runtime')?.summary.average_ms != null && <p className="text-sm">{t('modelMetrics.average')}: {t('modelMetrics.milliseconds', { value: Math.round(group.summary.other_duration.find((value) => value.kind === 'runtime')!.summary.average_ms!) })}</p>}
                    {!!group.summary.quantities.length && <Table><TableHeader><TableRow className="text-left border-b"><TableHead>{t('modelMetrics.metric')}</TableHead><TableHead>{t('modelMetrics.sample')}</TableHead><TableHead>{t('modelMetrics.quantitySum')}</TableHead></TableRow></TableHeader><TableBody>{group.summary.quantities.map((value) => <TableRow key={value.key} className="border-b"><TableCell className="py-2">{t(`modelMetrics.quantity.${value.key}`, { defaultValue: value.key })}</TableCell><TableCell className="font-mono">{decimalText(value.sample_count)}</TableCell><TableCell className="font-mono">{decimalText(value.sum)}</TableCell></TableRow>)}</TableBody></Table>}
                </div>)}
                {query.data.other && <p>{t('modelMetrics.runtimeOther')}</p>}
            </>}
        </>}
    </CardContent></Card>;
}
