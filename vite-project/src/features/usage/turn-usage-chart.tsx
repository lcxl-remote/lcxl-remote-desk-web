import { useTranslation } from 'react-i18next';
import {
    Table,
    TableBody,
    TableCell,
    TableHead,
    TableHeader,
    TableRow,
} from '@/components/ui/table';

/**
 * One aggregated TURN usage bucket. The `dimension` is the grouping key label
 * (a device code for the portable server, a user id for the manager console);
 * the component itself is dimension-agnostic so both consoles can reuse it.
 */
export interface TurnUsageRow {
    dimension: string;
    hourBucket: string;
    /** Relayed application data (ChannelData + Send/Data indications). Billable. */
    relayReceivedBytes: number;
    relaySentBytes: number;
    relayReceivedPkts: number;
    relaySentPkts: number;
    /** STUN + TURN control traffic. Observability only, never billed. */
    controlReceivedBytes: number;
    controlSentBytes: number;
    controlReceivedPkts: number;
    controlSentPkts: number;
}

export interface TurnUsageChartProps {
    /** Column header for the grouping dimension (e.g. "Device" / "User"). */
    dimensionLabel: string;
    rows: TurnUsageRow[];
}

/** Human-readable byte size. */
function formatBytes(bytes: number): string {
    if (bytes <= 0) {
        return '0 B';
    }
    const units = ['B', 'KB', 'MB', 'GB', 'TB'];
    const exp = Math.min(units.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)));
    const value = bytes / Math.pow(1024, exp);
    return `${value.toFixed(exp === 0 ? 0 : 2)} ${units[exp]}`;
}

interface DimensionTotals {
    dimension: string;
    relayReceivedBytes: number;
    relaySentBytes: number;
    relayPkts: number;
    controlBytes: number;
}

/**
 * Pure presentation of per-dimension TURN usage: a relative-magnitude bar plus a
 * totals table. Carries no data-fetching, so the web (by device) and manager (by
 * user) pages both feed it their own resolved rows.
 */
export function TurnUsageChart({ dimensionLabel, rows }: TurnUsageChartProps) {
    const { t } = useTranslation();

    // Aggregate the hourly rows up to per-dimension totals for the overview.
    const byDimension = new Map<string, DimensionTotals>();
    for (const row of rows) {
        const entry = byDimension.get(row.dimension) ?? {
            dimension: row.dimension,
            relayReceivedBytes: 0,
            relaySentBytes: 0,
            relayPkts: 0,
            controlBytes: 0,
        };
        entry.relayReceivedBytes += row.relayReceivedBytes;
        entry.relaySentBytes += row.relaySentBytes;
        entry.relayPkts += row.relayReceivedPkts + row.relaySentPkts;
        entry.controlBytes += row.controlReceivedBytes + row.controlSentBytes;
        byDimension.set(row.dimension, entry);
    }

    // Sort and scale the magnitude bar by billable (relay) traffic.
    const relayTotal = (d: DimensionTotals) => d.relayReceivedBytes + d.relaySentBytes;
    const totals = Array.from(byDimension.values()).sort(
        (a, b) => relayTotal(b) - relayTotal(a),
    );
    const maxTotal = totals.reduce((max, d) => Math.max(max, relayTotal(d)), 0);

    if (totals.length === 0) {
        return (
            <div className="text-muted-foreground text-sm py-8 text-center">
                {t('pages.turnUsage.empty')}
            </div>
        );
    }

    return (
        <div className="flex flex-col gap-4">
            <Table>
                <TableHeader>
                    <TableRow>
                        <TableHead>{dimensionLabel}</TableHead>
                        <TableHead>{t('pages.turnUsage.column.traffic')}</TableHead>
                        <TableHead className="text-right">
                            {t('pages.turnUsage.column.received')}
                        </TableHead>
                        <TableHead className="text-right">
                            {t('pages.turnUsage.column.sent')}
                        </TableHead>
                        <TableHead className="text-right">
                            {t('pages.turnUsage.column.control')}
                        </TableHead>
                        <TableHead className="text-right">
                            {t('pages.turnUsage.column.packets')}
                        </TableHead>
                    </TableRow>
                </TableHeader>
                <TableBody>
                    {totals.map((d) => {
                        const total = d.relayReceivedBytes + d.relaySentBytes;
                        const pct = maxTotal > 0 ? (total / maxTotal) * 100 : 0;
                        const rxPct = total > 0 ? (d.relayReceivedBytes / total) * 100 : 0;
                        return (
                            <TableRow key={d.dimension}>
                                <TableCell className="font-mono">{d.dimension}</TableCell>
                                <TableCell>
                                    <div className="h-3 w-40 rounded bg-muted overflow-hidden">
                                        <div
                                            className="h-full bg-primary/70"
                                            style={{ width: `${pct}%` }}
                                            title={`${formatBytes(total)} (${rxPct.toFixed(0)}% rx)`}
                                        />
                                    </div>
                                </TableCell>
                                <TableCell className="text-right">
                                    {formatBytes(d.relayReceivedBytes)}
                                </TableCell>
                                <TableCell className="text-right">
                                    {formatBytes(d.relaySentBytes)}
                                </TableCell>
                                <TableCell className="text-right text-muted-foreground">
                                    {formatBytes(d.controlBytes)}
                                </TableCell>
                                <TableCell className="text-right">
                                    {d.relayPkts.toLocaleString()}
                                </TableCell>
                            </TableRow>
                        );
                    })}
                </TableBody>
            </Table>
            <p className="text-muted-foreground text-xs">
                {t('pages.turnUsage.billableNote')}
            </p>
        </div>
    );
}
