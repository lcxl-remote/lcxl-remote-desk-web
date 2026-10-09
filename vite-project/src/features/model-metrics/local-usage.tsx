import { useEffect, useState } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { getModelUsage } from '@/services/clients';
import { UsageRangePicker, presetRange, type UsageRangeParams } from '@/features/usage/usage-range-picker';
import { ModelUsageChart } from '@/features/usage/model-usage-chart';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';

export function LocalModelUsage({ identity }: { identity: string }) {
    const { t } = useTranslation();
    const client = useQueryClient();
    const [epoch] = useState(() => globalThis.crypto?.randomUUID?.() ?? `${Date.now()}-${Math.random()}`);
    const [range, setRange] = useState<UsageRangeParams>(() => presetRange('24h', new Date()));
    const prefix = ['local-model-usage', location.origin, identity, epoch];
    const query = useQuery({ queryKey: [...prefix, range], queryFn: ({ signal }) => getModelUsage(range, { signal }), retry: false, gcTime: 0 });
    useEffect(() => () => { void client.cancelQueries({ queryKey: prefix }); client.removeQueries({ queryKey: prefix }); }, [client, identity, epoch]);
    const data = query.data?.data;
    const beforeCollection = !!data?.available_from && Date.parse(data.range.to) <= Date.parse(data.available_from);
    return <Card><CardHeader><CardTitle>{t('modelMetrics.tab.usage')}</CardTitle></CardHeader><CardContent className="space-y-4">
        <UsageRangePicker value={range} onChange={setRange} effective={data?.range}/>
        <Button variant="outline" disabled={query.isFetching} onClick={() => void query.refetch()}>{t('modelMetrics.refreshNow')}</Button>
        <p className="text-sm text-muted-foreground">{t('modelMetrics.usageHint')}</p>
        {query.isLoading && <p>{t('modelMetrics.loading')}</p>}{query.error && <p role="alert">{t('modelMetrics.queryFailed')}</p>}
        {data && !query.error && <><p>{t(`modelMetrics.state.${beforeCollection ? 'not_collected' : data.partial ? 'partial' : 'complete'}`)} · {t('modelMetrics.availableFrom')}: {data.available_from ?? '—'}</p>
            {beforeCollection ? <p>{t('modelMetrics.empty.notCollected')}</p> : !data.items.length ? <p>{t('modelMetrics.empty.noSamples')}</p> : <ModelUsageChart dimensionLabel={t('modelMetrics.model')} rows={data.items.map((row) => ({
                dimensionKey: JSON.stringify([row.providerId,row.modelId,row.purpose]),
                dimension: `${row.modelName} (#${row.modelId}) · ${t(`modelMetrics.value.purpose.${row.purpose}`, { defaultValue: row.purpose })}`,
                hourBucket: row.hourBucket,inputTokens: row.inputTokens,outputTokens: row.outputTokens,
                cacheReadTokens: row.cacheReadTokens,cacheWriteTokens: row.cacheWriteTokens,requestCount: row.requestCount,
            }))}/>}
        </>}
    </CardContent></Card>;
}
