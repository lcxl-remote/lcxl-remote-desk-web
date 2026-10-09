import { describe, expect, it } from 'vitest';
import { aggregateModelUsageRows, type ModelUsageRow } from './model-usage-chart';

function row(key: string, input: string): ModelUsageRow {
    return { dimensionKey: key, dimension: 'Same name', hourBucket: '2026-10-07T00:00:00Z', inputTokens: input, outputTokens: '0', cacheReadTokens: '0', cacheWriteTokens: '0', requestCount: '1' };
}

describe('exact usage totals', () => {
    it('preserves integers beyond Number.MAX_SAFE_INTEGER and keeps same-name models distinct', () => {
        const values = aggregateModelUsageRows([row('provider:1:model:1','9007199254740993'),row('provider:1:model:1','9'),row('provider:1:model:2','9007199254740993')]);
        expect(values).toHaveLength(2);
        expect(values[0].inputTokens.toString()).toBe('9007199254741002');
        expect(values[0].requestCount.toString()).toBe('2');
        expect(values[1].inputTokens.toString()).toBe('9007199254740993');
    });
});
