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
    hourBucket: string;
    inputTokens: number;
    outputTokens: number;
    cacheReadTokens: number;
    cacheWriteTokens: number;
    requestCount: number;
}

export interface ModelUsageChartProps {
    /** Column header for the grouping dimension (e.g. "Model"). */
    dimensionLabel: string;
    rows: ModelUsageRow[];
}

/** Human-readable token count (compact for large magnitudes). */
function formatTokens(tokens: number): string {
    if (tokens <= 0) {
        return '0';
    }
    if (tokens < 1000) {
        return tokens.toLocaleString();
    }
    const units = ['', 'K', 'M', 'B'];
    const exp = Math.min(units.length - 1, Math.floor(Math.log(tokens) / Math.log(1000)));
    const value = tokens / Math.pow(1000, exp);
    return `${value.toFixed(exp === 0 ? 0 : 2)}${units[exp]}`;
}

interface DimensionTotals {
    dimension: string;
    inputTokens: number;
    outputTokens: number;
    cacheReadTokens: number;
    cacheWriteTokens: number;
    requestCount: number;
}

/**
 * Pure presentation of per-dimension AI gateway token usage: a relative-magnitude
 * bar plus a totals table. Carries no data-fetching, so the web (by model) and
 * manager (by subject/model) pages both feed it their own resolved rows.
 */
export function ModelUsageChart({ dimensionLabel, rows }: ModelUsageChartProps) {
    const { t } = useTranslation();

    // Aggregate the hourly rows up to per-dimension totals for the overview.
    const byDimension = new Map<string, DimensionTotals>();
    for (const row of rows) {
        const entry = byDimension.get(row.dimension) ?? {
            dimension: row.dimension,
            inputTokens: 0,
            outputTokens: 0,
            cacheReadTokens: 0,
            cacheWriteTokens: 0,
            requestCount: 0,
        };
        entry.inputTokens += row.inputTokens;
        entry.outputTokens += row.outputTokens;
        entry.cacheReadTokens += row.cacheReadTokens;
        entry.cacheWriteTokens += row.cacheWriteTokens;
        entry.requestCount += row.requestCount;
        byDimension.set(row.dimension, entry);
    }

    const total = (d: DimensionTotals) =>
        d.inputTokens + d.outputTokens + d.cacheReadTokens + d.cacheWriteTokens;
    const totals = Array.from(byDimension.values()).sort((a, b) => total(b) - total(a));
    const maxTotal = totals.reduce((max, d) => Math.max(max, total(d)), 0);

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
                        const pct = maxTotal > 0 ? (sum / maxTotal) * 100 : 0;
                        const inPct = sum > 0 ? (d.inputTokens / sum) * 100 : 0;
                        return (
                            <TableRow key={d.dimension}>
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
                                    {d.requestCount.toLocaleString()}
                                </TableCell>
                            </TableRow>
                        );
                    })}
                </TableBody>
            </Table>
        </div>
    );
}
