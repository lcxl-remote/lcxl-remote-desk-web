import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Bot } from 'lucide-react';

import { useGetModelUsage } from '@/services/hooks/modelUsageController/useGetModelUsage';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { ModelUsageChart, type ModelUsageRow } from '@/features/usage/model-usage-chart';
import {
    UsageRangePicker,
    presetRange,
    type UsageRangeParams,
} from '@/features/usage/usage-range-picker';

/**
 * Local per-model AI gateway token-usage view for the portable/signal server.
 * Collect-only telemetry (no billing); usage is attributed by model name, the
 * only meaningful dimension when there is a single local owner.
 */
export function ModelUsagePage() {
    const { t } = useTranslation();
    const [range, setRange] = useState<UsageRangeParams>(() => presetRange('24h', new Date()));
    const { data, isLoading, error } = useGetModelUsage({ from: range.from, to: range.to });

    const effective = data?.data?.range;
    const items = data?.data?.items ?? [];
    const rows: ModelUsageRow[] = items.map((item) => ({
        dimension: item.modelName,
        hourBucket: item.hourBucket,
        inputTokens: item.inputTokens,
        outputTokens: item.outputTokens,
        cacheReadTokens: item.cacheReadTokens,
        cacheWriteTokens: item.cacheWriteTokens,
        requestCount: item.requestCount,
    }));

    return (
        <div className="flex flex-col gap-6 p-6 max-w-4xl mx-auto w-full">
            <Card>
                <CardHeader>
                    <div className="flex items-center gap-2">
                        <Bot className="h-5 w-5 text-primary" />
                        <CardTitle>{t('pages.modelUsage.title')}</CardTitle>
                    </div>
                    <CardDescription>{t('pages.modelUsage.description')}</CardDescription>
                </CardHeader>
                <CardContent className="flex flex-col gap-4">
                    <UsageRangePicker value={range} onChange={setRange} effective={effective} />
                    {isLoading && (
                        <div className="text-muted-foreground text-sm py-8 text-center">
                            {t('pages.modelUsage.loading')}
                        </div>
                    )}
                    {error && (
                        <div className="text-destructive text-sm py-8 text-center">
                            {t('pages.modelUsage.error')}
                        </div>
                    )}
                    {!isLoading && !error && (
                        <ModelUsageChart
                            dimensionLabel={t('pages.modelUsage.column.model')}
                            rows={rows}
                        />
                    )}
                </CardContent>
            </Card>
        </div>
    );
}
