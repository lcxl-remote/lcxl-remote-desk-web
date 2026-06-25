import { useTranslation } from 'react-i18next';
import { useQuery } from '@tanstack/react-query';
import { Network } from 'lucide-react';

import { axiosInstance } from '@/lib/kubb-client';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { TurnUsageChart, type TurnUsageRow } from '@/features/settings/turn-usage-chart';

/** One per-device usage bucket from `GET /api/turn/usage` (camelCase body). */
interface TurnUsageItem {
    deviceCode: string;
    hourBucket: string;
    receivedBytes: number;
    sentBytes: number;
    receivedPkts: number;
    sentPkts: number;
}

interface TurnUsageResponse {
    data?: { items?: TurnUsageItem[] };
}

/**
 * Local per-device TURN usage view for the portable/signal server. Collect-only
 * telemetry (no billing); attribution is by device code, falling back to the raw
 * connection id for connections that never resolved to a device.
 */
export function TurnUsagePage() {
    const { t } = useTranslation();
    const { data, isLoading, error } = useQuery({
        queryKey: ['turn-usage'],
        queryFn: async () => {
            const res = await axiosInstance.get<TurnUsageResponse>('/api/turn/usage');
            return res.data;
        },
    });

    const items = data?.data?.items ?? [];
    const rows: TurnUsageRow[] = items.map((item) => ({
        dimension: item.deviceCode,
        hourBucket: item.hourBucket,
        receivedBytes: item.receivedBytes,
        sentBytes: item.sentBytes,
        receivedPkts: item.receivedPkts,
        sentPkts: item.sentPkts,
    }));

    return (
        <div className="flex flex-col gap-6 p-6 max-w-4xl mx-auto w-full">
            <Card>
                <CardHeader>
                    <div className="flex items-center gap-2">
                        <Network className="h-5 w-5 text-primary" />
                        <CardTitle>{t('pages.turnUsage.title')}</CardTitle>
                    </div>
                    <CardDescription>{t('pages.turnUsage.description')}</CardDescription>
                </CardHeader>
                <CardContent>
                    {isLoading && (
                        <div className="text-muted-foreground text-sm py-8 text-center">
                            {t('pages.turnUsage.loading')}
                        </div>
                    )}
                    {error && (
                        <div className="text-destructive text-sm py-8 text-center">
                            {t('pages.turnUsage.error')}
                        </div>
                    )}
                    {!isLoading && !error && (
                        <TurnUsageChart
                            dimensionLabel={t('pages.turnUsage.column.device')}
                            rows={rows}
                        />
                    )}
                </CardContent>
            </Card>
        </div>
    );
}
