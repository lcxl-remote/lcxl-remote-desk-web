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
 * One aggregated AI gateway usage bucket. The `dimension` is the grouping key
 * label (a model name for the portable server; a subject/model on the manager
 * console); the component itself is dimension-agnostic so both consoles reuse
 * it. Token classes mirror the backend rollup: non-cached input, output, cache
 * read, cache write — cache is split out because it bills at very different
 * rates.
 */
export interface ModelUsageRow {
    dimension: string;
    dimensionKey: string;
    hourBucket: string;
    inputTokens: string;
    outputTokens: string;
    cacheReadTokens: string;
    cacheWriteTokens: string;
    requestCount: string;
}

export interface ModelUsageChartProps {
    /** Column header for the grouping dimension (e.g. "Model"). */
    dimensionLabel: string;
    rows: ModelUsageRow[];
}

function formatTokens(tokens: bigint): string {
    return tokens.toString().replace(/\B(?=(\d{3})+(?!\d))/g, ',');
}

export interface DimensionTotals {
    dimension: string;
    dimensionKey: string;
    inputTokens: bigint;
    outputTokens: bigint;
    cacheReadTokens: bigint;
    cacheWriteTokens: bigint;
    requestCount: bigint;
}

export function aggregateModelUsageRows(rows: ModelUsageRow[]): DimensionTotals[] {
    const byDimension = new Map<string,DimensionTotals>();
    for (const row of rows) {
        const entry = byDimension.get(row.dimensionKey) ?? {
            dimension: row.dimension, dimensionKey: row.dimensionKey,
            inputTokens: 0n, outputTokens: 0n, cacheReadTokens: 0n, cacheWriteTokens: 0n, requestCount: 0n,
        };
        entry.inputTokens += BigInt(row.inputTokens);
        entry.outputTokens += BigInt(row.outputTokens);
        entry.cacheReadTokens += BigInt(row.cacheReadTokens);
        entry.cacheWriteTokens += BigInt(row.cacheWriteTokens);
        entry.requestCount += BigInt(row.requestCount);
        byDimension.set(row.dimensionKey,entry);
    }
    return Array.from(byDimension.values()).sort((a,b) => total(a)<total(b) ? 1 : total(a)>total(b) ? -1 : a.dimensionKey.localeCompare(b.dimensionKey));
}
function total(d: DimensionTotals): bigint { return d.inputTokens+d.outputTokens+d.cacheReadTokens+d.cacheWriteTokens; }

/**
 * Pure presentation of per-dimension AI gateway token usage: a relative-magnitude
 * bar plus a totals table. Carries no data-fetching, so the web (by model) and
 * manager (by subject/model) pages both feed it their own resolved rows.
 */
export function ModelUsageChart({ dimensionLabel, rows }: ModelUsageChartProps) {
    const { t } = useTranslation();

    const totals = aggregateModelUsageRows(rows);
    const maxTotal = totals.reduce((max,d) => total(d)>max ? total(d) : max, 0n);

    if (totals.length === 0) {
        return (
            <div className="text-muted-foreground text-sm py-8 text-center">
                {t('pages.modelUsage.empty')}
            </div>
        );
    }

    return (
        <div className="flex flex-col gap-4">
            <Table>
                <TableHeader>
                    <TableRow>
                        <TableHead>{dimensionLabel}</TableHead>
                        <TableHead>{t('pages.modelUsage.column.tokens')}</TableHead>
                        <TableHead className="text-right">
                            {t('pages.modelUsage.column.input')}
                        </TableHead>
                        <TableHead className="text-right">
                            {t('pages.modelUsage.column.output')}
                        </TableHead>
                        <TableHead className="text-right">
                            {t('pages.modelUsage.column.cacheRead')}
                        </TableHead>
                        <TableHead className="text-right">
                            {t('pages.modelUsage.column.cacheWrite')}
                        </TableHead>
                        <TableHead className="text-right">
                            {t('pages.modelUsage.column.requests')}
                        </TableHead>
                    </TableRow>
                </TableHeader>
                <TableBody>
                    {totals.map((d) => {
                        const sum = total(d);
                        const pct = maxTotal > 0n ? Number(sum) / Number(maxTotal) * 100 : 0;
                        const inPct = sum > 0n ? Number(d.inputTokens) / Number(sum) * 100 : 0;
                        return (
                            <TableRow key={d.dimensionKey}>
                                <TableCell className="font-mono">{d.dimension}</TableCell>
                                <TableCell>
                                    <div className="h-3 w-40 rounded bg-muted overflow-hidden">
                                        <div
                                            className="h-full bg-primary/70"
                                            style={{ width: `${pct}%` }}
                                            title={`${formatTokens(sum)} (${inPct.toFixed(0)}% in)`}
                                        />
                                    </div>
                                </TableCell>
                                <TableCell className="text-right">
                                    {formatTokens(d.inputTokens)}
                                </TableCell>
                                <TableCell className="text-right">
                                    {formatTokens(d.outputTokens)}
                                </TableCell>
                                <TableCell className="text-right">
                                    {formatTokens(d.cacheReadTokens)}
                                </TableCell>
                                <TableCell className="text-right">
                                    {formatTokens(d.cacheWriteTokens)}
                                </TableCell>
                                <TableCell className="text-right">
                                    {formatTokens(d.requestCount)}
                                </TableCell>
                            </TableRow>
                        );
                    })}
                </TableBody>
            </Table>
        </div>
    );
}
