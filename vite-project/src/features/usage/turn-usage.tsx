import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Network } from 'lucide-react';

import { useGetTurnUsage } from '@/services/hooks/turnUsageController/useGetTurnUsage';
import { useGetTurnInfo } from '@/services/hooks/turnController/useGetTurnInfo';
import { turnRuntimeStateEnum } from '@/services/types';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { TurnUsageChart, type TurnUsageRow } from '@/features/usage/turn-usage-chart';
import {
    UsageRangePicker,
    presetRange,
    type UsageRangeParams,
} from '@/features/usage/usage-range-picker';

/**
 * Local per-device TURN usage view for the portable/signal server. Collect-only
 * telemetry (no billing); attribution is by device code, falling back to the raw
 * connection id for connections that never resolved to a device.
 */
export function TurnUsagePage() {
    const { t } = useTranslation();
    const [range, setRange] = useState<UsageRangeParams>(() => presetRange('24h', new Date()));
    const { data, isLoading, error } = useGetTurnUsage({ from: range.from, to: range.to });

    const effective = data?.data?.range;
    const items = data?.data?.items ?? [];
    const rows: TurnUsageRow[] = items.map((item) => ({
        dimension: item.deviceCode,
        hourBucket: item.hourBucket,
        relayReceivedBytes: item.relayReceivedBytes,
        relaySentBytes: item.relaySentBytes,
        relayReceivedPkts: item.relayReceivedPkts,
        relaySentPkts: item.relaySentPkts,
        controlReceivedBytes: item.controlReceivedBytes,
        controlSentBytes: item.controlSentBytes,
        controlReceivedPkts: item.controlReceivedPkts,
        controlSentPkts: item.controlSentPkts,
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
                <CardContent className="flex flex-col gap-4">
                    <UsageRangePicker value={range} onChange={setRange} effective={effective} />
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
                    {/* An empty history is normal on a host with no relay of its
                        own, and used to be indistinguishable from a broken page.
                        The runtime state says which it is. */}
                    {!isLoading && !error && rows.length === 0 && <NoRelayHint />}
                </CardContent>
            </Card>
        </div>
    );
}

/**
 * Why there is nothing to show.
 *
 * The history is kept whether or not this host relays, so an empty range on a
 * host that has never had a relay is not a gap in the data — there was never
 * any traffic to record, and saying so is more useful than an empty chart.
 */
function NoRelayHint() {
    const { t } = useTranslation();
    const { data } = useGetTurnInfo();
    const state = data?.data?.state;
    if (!state || state === turnRuntimeStateEnum.running) {
        return null;
    }
    return (
        <p className="text-muted-foreground text-sm text-center">
            {t(
                state === turnRuntimeStateEnum.unsupported
                    ? 'pages.turnUsage.noRelay.unsupported'
                    : 'pages.turnUsage.noRelay.notRelaying',
            )}
        </p>
    );
}
