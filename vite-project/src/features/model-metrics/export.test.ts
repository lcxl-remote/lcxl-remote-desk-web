import { describe, expect, it } from 'vitest';
import { aggregateCsv, callsCsv } from './export';
import { coverage, groups, recordFixture, summary } from './test-fixtures';

describe('bounded, content-free metric CSV', () => {
    it('exports only approved current-page fields with exact tokens and coverage', () => {
        const record = { ...recordFixture(), model_name: '=SUM(1,2)', prompt: 'private-prompt', actor_id: 'private-actor', device_id: 'private-device', response: 'private-response' };
        const value = callsCsv({ coverage: coverage('partial'), records: [record], next_cursor: 'private-next-cursor', received_before: '2026-10-09T00:00:00Z' }, { tool: 'read_file', contract_revision: '1', limit: 50, cursor: 'private-page-cursor' });
        expect(value.split('\r\n')).toHaveLength(3);
        expect(value).toContain('"current_call_page"');
        expect(value).toContain('"9007199254740993"');
        expect(value).toContain('"partial"');
        expect(value).toContain('"schema:failed"');
        expect(value).toContain('"tool_counts_status"');
        expect(value).toContain('"\'=SUM(1,2)"');
        expect(value).toContain('{""tool"":""read_file"",""contract_revision"":""1""}');
        for (const text of ['private-', '9007199254740992', 'actor_id', 'device_id', 'prompt', 'response', 'correction_group_root', 'correction_of', 'call_id']) expect(value).not.toContain(text);
    });

    it('exports record-only conditions and collected-count completeness without cursors', () => {
        const record = { ...recordFixture(), kind: 'call', tool_count: '1', input_rejected_count: '1', generated_tool_count: '2', tool_counts_status: 'partial' as const };
        const value = callsCsv({ coverage: coverage(), records: [record], next_cursor: null, received_before: '2026-10-09T00:00:00Z' }, { record_kind: 'call', outcome: 'request_error', min_duration_ms: 5000, cursor: 'secret-cursor' });
        expect(value).toContain('""record_kind"":""call""'); expect(value).toContain('""min_duration_ms"":5000');
        expect(value).toContain('"input_rejected_count"'); expect(value).toContain('"partial"'); expect(value).not.toContain('secret-cursor');
    });

    it('retains range and empty-page state without inventing a record', () => {
        const value = callsCsv({ coverage: coverage('not_collected'), records: [], next_cursor: null, received_before: '2026-10-09T00:00:00Z' }, {});
        expect(value.split('\r\n')).toHaveLength(2);
        expect(value).toContain('"not_collected"');
        expect(value).toContain('"2026-10-08T00:00:00Z"');
        expect(value).toContain('"context"');
        expect(value).not.toContain('"record"');
    });

    it('exports group and other aggregates with qualified denominators and latency', () => {
        const value = groups('read_file');
        value.other = summary();
        value.groups[0].summary.counts[0].count = '9007199254740993';
        const result = aggregateCsv(value, { error: 'input.type' });
        expect(result).toContain('"provider-a:model-a"');
        expect(result).toContain('"other"');
        expect(result).toContain('"returned_or_known_request_error"');
        expect(result).toContain('"latency"');
        expect(result).toContain('"9007199254740993"');
        expect(result).toContain('"current_filtered_aggregates"');
        expect(result).toContain('{""error"":""input.type""}');
    });
});
