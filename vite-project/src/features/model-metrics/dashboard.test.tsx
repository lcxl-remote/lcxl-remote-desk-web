import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { MetricsDashboard } from './dashboard';
import { metricTime } from './format';
import { MetricsRuntimePanel } from './runtime-panel';
import { apiFixture, coverage, groups, recordFixture, summary } from './test-fixtures';
import type { MetricsApi } from './dashboard';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then((module) => module.reactI18nextMock()));
const clients: QueryClient[] = [];
afterEach(() => { cleanup(); clients.splice(0).forEach((client) => client.clear()); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

function mount(api: MetricsApi, props: { allowed?: boolean; ledger?: ReactNode } = {}) {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    clients.push(client);
    return { client, ...render(<QueryClientProvider client={client}><MetricsDashboard api={api} identity="owner:one" allowed={props.allowed ?? true} ledger={props.ledger}/></QueryClientProvider>) };
}

describe('model metric dashboard isolation and attribution', () => {
    it('keeps collection forms on the settings page and shows local status and detail times', async () => {
        const api = apiFixture();
        vi.mocked(api.calls).mockResolvedValue({ coverage: coverage(), records: [recordFixture()], next_cursor: null, received_before: '2026-10-09T00:00:00Z' });
        const view = mount(api);
        await waitFor(() => expect(api.status).toHaveBeenCalled());
        expect(screen.queryByRole('tab', { name: 'Collection settings' })).toBeNull();
        expect(api.settings).not.toHaveBeenCalled();
        await waitFor(() => expect(view.container.querySelector('time')?.textContent).toBe(metricTime('2026-10-09T00:00:00Z', 'en-US')));
        fireEvent.click(screen.getByRole('tab', { name: 'Calls' }));
        await screen.findByText('Selected model');
        expect(view.container.querySelector('tbody time')?.textContent).toBe(metricTime('2026-10-08T00:00:00Z', 'en-US'));
    });

    it('labels a single failed sample without presenting it as a high-volume error signal', async () => {
        const api = apiFixture(); const value = summary();
        value.rates[0] = { ...value.rates[0], numerator: '1', denominator: '1', value: 1 };
        vi.mocked(api.overview).mockResolvedValue({ coverage: coverage(), summary: value, previous: null });
        mount(api);
        await screen.findByText('One qualified sample');
        expect(screen.getByText('100.00%')).toBeTruthy();
        expect(screen.queryByText('At least half of qualified outcomes failed or were rejected')).toBeNull();
    });

    it('keeps unknown-operation KPI and operation latency drill-down within actual native dispatches', async () => {
        const api = apiFixture(); const value = summary();
        value.counts = [{ key: 'operations_unknown', count: '1' }];
        value.other_duration = [{ kind: 'operation', summary: { ...value.duration, count: '1', p95_ms: 500 } }];
        vi.mocked(api.overview).mockResolvedValue({ coverage: coverage(), summary: value, previous: null });
        mount(api);
        fireEvent.click(await screen.findByRole('button', { name: 'View calls: Operations Unknown' }));
        await waitFor(() => expect(vi.mocked(api.calls).mock.calls.at(-1)![0]).toMatchObject({ record_kind: 'operation', outcome: 'unknown', dispatched: true }));
        fireEvent.click(screen.getByRole('tab', { name: 'Overview' }));
        fireEvent.click(await screen.findByRole('button', { name: 'At or above estimated P95' }));
        await waitFor(() => expect(vi.mocked(api.calls).mock.calls.at(-1)![0]).toMatchObject({ record_kind: 'operation', dispatched: true, min_duration_ms: 500, latency: 'duration' }));
        expect(vi.mocked(api.calls).mock.calls.at(-1)![0].outcome).toBeUndefined();
    });

    it('drills KPI conclusions into records without changing aggregate queries', async () => {
        const api = apiFixture();
        const value = summary();
        value.counts = [{ key: 'calls', count: '4' }, { key: 'request_errors', count: '1' }, { key: 'input_rejected', count: '2' }];
        vi.mocked(api.overview).mockResolvedValue({ coverage: coverage(), summary: value, previous: null });
        mount(api);
        fireEvent.click(await screen.findByRole('button', { name: 'View calls: Request Errors' }));
        await waitFor(() => expect(api.calls).toHaveBeenCalled());
        expect(vi.mocked(api.calls).mock.calls.at(-1)![0]).toMatchObject({ record_kind: 'call', outcome: 'request_error' });
        fireEvent.click(screen.getByRole('button', { name: 'Slow: at least 5 seconds' }));
        await waitFor(() => expect(vi.mocked(api.calls).mock.calls.at(-1)![0].min_duration_ms).toBe(5000));
        fireEvent.click(screen.getByRole('tab', { name: 'Overview' }));
        await waitFor(() => expect(api.overview).toHaveBeenCalledTimes(2));
        expect(vi.mocked(api.overview).mock.calls.at(-1)![0].outcome).toBeUndefined();
        expect(vi.mocked(api.overview).mock.calls.at(-1)![0].min_duration_ms).toBeUndefined();
        fireEvent.click(await screen.findByRole('button', { name: 'View calls: Input Rejected' }));
        await waitFor(() => expect(vi.mocked(api.calls).mock.calls.at(-1)![0]).toMatchObject({ record_kind: 'tool', outcome: 'rejected' }));
        expect(vi.mocked(api.calls).mock.calls.at(-1)![0].min_duration_ms).toBeUndefined();
    });

    it('shows collected child count coverage and exact observed Token values on call rows', async () => {
        const api = apiFixture();
        const row = { ...recordFixture(), kind: 'call', tool: null, outcome: 'returned', input_tokens: '9007199254740993', output_tokens: '7', generated_tool_count: '2', tool_count: '1', input_rejected_count: '1', tool_counts_status: 'partial' as const, usage_complete: true };
        vi.mocked(api.calls).mockResolvedValue({ coverage: coverage(), records: [row], next_cursor: null, received_before: '2026-10-09T00:00:00Z' });
        mount(api);
        fireEvent.click(screen.getByRole('tab', { name: 'Calls' }));
        const name = await screen.findByText('Selected model');
        const line = name.closest('tr')!;
        expect(line.textContent).toContain('provider-1 / model-1');
        expect(line.textContent).toContain('1 / 1');
        expect(line.textContent).toContain('Generated: 2');
        expect(line.textContent).toContain('Partial');
        expect(line.textContent).toContain('9,007,199,254,740,993 / 7');
    });

    it('passes grouping sort to the server and drills an associated model within the original tool', async () => {
        const api = apiFixture();
        const value = groups('read_file');
        value.groups[0].associated_models = [{ provider_id: 'provider-2', model_id: 'model-2', model_name: 'Second model', tool_inputs: '9007199254740993' }];
        value.groups[0].summary.schema_paths = [{ path: '$.items[].count', count: '9007199254740993' }];
        value.groups[0].summary.other_schema_errors = '1'; value.groups[0].summary.schema_paths_limited = true;
        value.groups[0].summary.stages = [{ stage: 'schema', outcome: 'failed', count: '1' }];
        vi.mocked(api.tools).mockResolvedValue(value);
        mount(api);
        fireEvent.click(screen.getByRole('tab', { name: 'Tool inputs / references / correction' }));
        await screen.findByText('$.items[].count');
        expect(screen.getByText(/Only some parameter fields are shown/)).toBeTruthy();
        expect(screen.getByText(/Second model · provider-2 \/ model-2/).textContent).toContain('9,007,199,254,740,993');
        fireEvent.change(screen.getByRole('combobox', { name: 'Sort groups before Top N' }), { target: { value: 'input_rejection_rate' } });
        await waitFor(() => expect(vi.mocked(api.tools).mock.calls.at(-1)![0].group_sort).toBe('input_rejection_rate'));
        fireEvent.click((await screen.findByText(/Second model · provider-2 \/ model-2/)).closest('div')!.querySelector('button')!);
        await waitFor(() => expect(api.calls).toHaveBeenCalled());
        expect(vi.mocked(api.calls).mock.calls.at(-1)![0]).toMatchObject({ tool: 'read_file', provider_id: 'provider-2', model_id: 'model-2' });
        expect(vi.mocked(api.calls).mock.calls.at(-1)![0].group_sort).toBeUndefined();
    });

    it('shows P95 latency with sample counts and preserves gaps instead of connecting missing buckets', async () => {
        const api = apiFixture();
        const point = (count: string, p95: number | null) => { const value = summary(); value.duration = { ...value.duration, count, p95_ms: p95 }; return value; };
        vi.mocked(api.series).mockResolvedValue({ coverage: coverage('partial'), points: [
            { bucket: '2026-10-08T00:00:00Z', summary: point('1', 500) },
            { bucket: '2026-10-08T01:00:00Z', summary: point('0', null) },
            { bucket: '2026-10-08T02:00:00Z', summary: point('3', 700) },
        ] });
        mount(api);
        const selection = await screen.findByRole('combobox', { name: 'Metric' });
        fireEvent.change(selection, { target: { value: 'duration' } });
        const plot = screen.getByRole('img', { name: 'Trend' });
        expect(plot.querySelectorAll('circle')).toHaveLength(2);
        expect(plot.querySelectorAll('line')).toHaveLength(0);
        expect(plot.textContent).toContain('500 ms · 1');
        fireEvent.click(screen.getByText('Show exact counts'));
        expect(screen.getByText(metricTime('2026-10-08T01:00:00Z', 'en-US')).closest('tr')!.textContent).toContain('—');
    });

    it('exports the displayed calls page without fetching another page and disables export after a query failure', async () => {
        const api = apiFixture();
        vi.mocked(api.calls).mockResolvedValueOnce({ coverage: coverage('partial'), records: [recordFixture()], next_cursor: 'next', received_before: '2026-10-09T00:00:00Z' }).mockRejectedValue(new Error('Unavailable'));
        const blobs: Blob[] = [];
        vi.stubGlobal('URL', class extends URL {
            static createObjectURL = vi.fn((blob: Blob) => { blobs.push(blob); return 'blob:fixture'; });
            static revokeObjectURL = vi.fn();
        });
        const click = vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(function () {
            expect(this.isConnected).toBe(true);
            expect(URL.revokeObjectURL).not.toHaveBeenCalled();
        });
        mount(api);
        fireEvent.click(screen.getByRole('tab', { name: 'Calls' }));
        await screen.findByText('Selected model');
        const button = screen.getByRole('button', { name: 'Export current page CSV' });
        await waitFor(() => expect((button as HTMLButtonElement).disabled).toBe(false));
        fireEvent.click(button);
        expect(api.calls).toHaveBeenCalledTimes(1);
        expect(click).toHaveBeenCalledTimes(1);
        expect(click.mock.instances[0].download).toBe('model-metrics-calls.csv');
        expect(click.mock.instances[0].isConnected).toBe(false);
        expect(URL.revokeObjectURL).not.toHaveBeenCalled();
        expect(blobs).toHaveLength(1);
        fireEvent.click(screen.getByRole('button', { name: 'Refresh now' }));
        await screen.findByRole('alert');
        expect((button as HTMLButtonElement).disabled).toBe(true);
        fireEvent.click(button);
        expect(blobs).toHaveLength(1);
    });

    it('keeps unresolved model groups out of model choices and prevents drilling into all models', async () => {
        const api = apiFixture();
        const unsent = summary();
        unsent.counts = [{ key: 'calls', count: '1' }, { key: 'not_started', count: '1' }];
        unsent.rates = unsent.rates.map((rate) => ({ ...rate, numerator: '0', denominator: '0', value: null, sample_status: 'no_samples' }));
        vi.mocked(api.models).mockResolvedValue({ coverage: coverage(), other: null, groups: [{
            key: ':', provider_id: null, model_id: null, model_name: null, tool: null,
            summary: unsent, associated_models: [], other_model_count: '0', configurations: [], configurations_limited: false,
        }] });
        mount(api);
        fireEvent.click(screen.getByRole('tab', { name: 'Models' }));
        await screen.findByText(/No model identity was resolved/);
        const button = screen.getByRole('button', { name: 'View calls' });
        expect((button as HTMLButtonElement).disabled).toBe(true);
        fireEvent.click(button);
        expect(api.calls).not.toHaveBeenCalled();
        expect(screen.queryByText('0.00%')).toBeNull();
        expect((screen.getByRole('combobox', { name: 'Model' }) as HTMLSelectElement).options.length).toBe(1);
    });
    it('lists missing facts by reception time without model filters or invented rates', async () => {
        const api = apiFixture();
        vi.mocked(api.unassociated).mockResolvedValue({
            coverage: { ...coverage('partial'), cohort_basis: 'fact_received' }, next_cursor: 'gap-next',
            records: [{
                id: 'unassociated.server-generated-id', kind: 'operation', phase: 'completed',
                started_at: null, received_at: '2026-10-08T02:00:00Z', occurred_at: '2026-10-08T01:59:00Z', updated_at: '2026-10-08T02:00:01Z',
                missing: 'attribution', state: 'unavailable', original_model: null, call_id: null,
                tool_observation_id: null, tool: null, ordinal: 0, permission: null, fact_outcome: 'verified',
            }],
        });
        mount(api);
        fireEvent.click(screen.getByRole('tab', { name: 'Tool inputs / references / correction' }));
        fireEvent.click(await screen.findByRole('button', { name: 'View calls' }));
        await waitFor(() => expect(api.calls).toHaveBeenCalled());
        fireEvent.click(screen.getByRole('tab', { name: 'Unassociated facts' }));
        await screen.findAllByText(metricTime('2026-10-08T02:00:00Z', 'en-US'));
        const request = vi.mocked(api.unassociated).mock.calls.at(-1)![0];
        expect(Object.keys(request).sort()).toEqual(['cursor', 'from', 'limit', 'to']);
        expect(request.from).toBeDefined(); expect(request.to).toBeDefined();
        expect(screen.queryByText('0.00%')).toBeNull();
        expect(screen.queryByText('Same name')).toBeNull();
        expect(screen.getAllByText('Not observed').length).toBeGreaterThan(0);
        expect(screen.getByText('Original attribution')).toBeTruthy();
        expect(screen.getByText('Association unavailable')).toBeTruthy();
        expect(screen.queryByRole('button', { name: 'View original input' })).toBeNull();
        fireEvent.click(screen.getByRole('button', { name: 'Next page' }));
        await waitFor(() => expect(vi.mocked(api.unassociated).mock.calls.at(-1)![0].cursor).toBe('gap-next'));
    });

    it('clears unassociated data when the backend revokes metrics access', async () => {
        const api = apiFixture();
        vi.mocked(api.unassociated).mockResolvedValueOnce({
            coverage: { ...coverage('partial'), cohort_basis: 'fact_received' }, next_cursor: null,
            records: [{
                id: 'retained-server-gap', kind: 'tool', phase: 'completed', started_at: null,
                received_at: '2026-10-08T02:00:00Z', occurred_at: '2026-10-08T02:00:00Z', updated_at: '2026-10-08T02:00:00Z',
                missing: 'attribution', state: 'unavailable', original_model: null, call_id: null,
                tool_observation_id: null, tool: null, ordinal: null, permission: 'cancelled', fact_outcome: null,
            }],
        }).mockRejectedValue({ fixtureAccessDenied: true });
        const { client } = mount(api);
        fireEvent.click(screen.getByRole('tab', { name: 'Unassociated facts' }));
        await screen.findByText('retained-server-gap');
        fireEvent.click(screen.getByRole('button', { name: 'Refresh now' }));
        await screen.findByRole('alert');
        await waitFor(() => expect(client.getQueryCache().findAll({ queryKey: ['model-metrics'] }).every((query) => query.state.data === undefined)).toBe(true));
        expect(screen.queryByText('retained-server-gap')).toBeNull();
        expect(screen.queryByRole('tablist')).toBeNull();
    });

    it('shows distinct permission conclusions with exact integer values', async () => {
        const api = apiFixture();
        const value = summary();
        value.counts = [
            { key: 'permission_approved', count: '1' },
            { key: 'permission_narrowed', count: '2' },
            { key: 'permission_denied', count: '3' },
            { key: 'permission_revoked', count: '4' },
            { key: 'permission_policy_rejected', count: '9007199254740993' },
            { key: 'permission_unavailable', count: '5' },
        ];
        vi.mocked(api.overview).mockResolvedValue({ coverage: coverage(), summary: value, previous: null });
        mount(api);
        fireEvent.click(await screen.findByText('All counts'));
        for (const [label, count] of [
            ['Permission Approved', '1'], ['Permission narrowed', '2'],
            ['Permission denied by owner', '3'], ['Permission revoked', '4'],
            ['Permission blocked by policy', '9,007,199,254,740,993'],
            ['No trustworthy permission verdict', '5'],
        ]) {
            const heading = screen.getByText(label);
            expect(heading.closest('div')!.querySelector('dd')!.textContent).toBe(count);
        }
        expect(screen.queryByText('permission_narrowed')).toBeNull();
        expect(screen.queryByText('permission_revoked')).toBeNull();
        expect(screen.queryByText('permission_policy_rejected')).toBeNull();
    });

    it('does not issue metrics requests for an unauthorized context', () => {
        const api = apiFixture();
        mount(api, { allowed: false });
        expect(api.status).not.toHaveBeenCalled();
        expect(api.overview).not.toHaveBeenCalled();
        expect(api.models).not.toHaveBeenCalled();
    });

    it('keeps a tool group across all models when opening its calls', async () => {
        const api = apiFixture();
        mount(api);
        fireEvent.click(screen.getByRole('tab', { name: 'Tool inputs / references / correction' }));
        fireEvent.click(await screen.findByRole('button', { name: 'View calls' }));
        await waitFor(() => expect(api.calls).toHaveBeenCalled());
        const request = vi.mocked(api.calls).mock.calls.at(-1)![0];
        expect(request.tool).toBe('read_file');
        expect(request.provider_id).toBeUndefined();
        expect(request.model_id).toBeUndefined();
    });

    it('does not display collected zero counts for a pre-activation range', async () => {
        const api = apiFixture();
        vi.mocked(api.overview).mockResolvedValue({ coverage: coverage('not_collected'), summary: summary(), previous: null });
        mount(api);
        await screen.findByText('Statistics were not enabled during this period. No data is available.');
        expect(screen.queryByText('0.00%')).toBeNull();
        expect(screen.queryByText('All counts')).toBeNull();
    });

    it('keeps the independent financial ledger usable during an observation outage', async () => {
        const api = apiFixture();
        vi.mocked(api.status).mockRejectedValue(new Error('Observation store unavailable'));
        vi.mocked(api.overview).mockRejectedValue(new Error('Observation store unavailable'));
        mount(api, { ledger: <div>Retained financial ledger</div> });
        fireEvent.click(screen.getByRole('tab', { name: 'Usage ledger' }));
        expect(screen.getByText('Retained financial ledger')).toBeTruthy();
        expect(screen.queryByText('Time range')).toBeNull();
    });

    it('passes the displayed range to runtime observations and rejects unsupported model filters locally', async () => {
        const api = apiFixture();
        const client = new QueryClient(); clients.push(client);
        const filters = { from: '2026-10-08T00:00:00Z', to: '2026-10-09T00:00:00Z', origin: 'subagent', granularity: 'five_minutes' as const };
        const view = render(<QueryClientProvider client={client}><MetricsRuntimePanel api={api} identity="platform:one" allowed queryFilters={filters} autoRefresh={false}/></QueryClientProvider>);
        await waitFor(() => expect(api.runtime).toHaveBeenCalled());
        expect(vi.mocked(api.runtime).mock.calls[0][0]).toMatchObject(filters);
        const requests = vi.mocked(api.runtime).mock.calls.length;
        view.rerender(<QueryClientProvider client={client}><MetricsRuntimePanel api={api} identity="platform:one" allowed queryFilters={{ ...filters, model_id: 'model-a' }} autoRefresh={false}/></QueryClientProvider>);
        expect(screen.getByText(/Runtime observations do not support these filters/)).toBeTruthy();
        expect(vi.mocked(api.runtime).mock.calls.length).toBe(requests);
    });

    it('removes cached observations when the authenticated identity changes', async () => {
        const api = apiFixture();
        const first = summary(); first.counts = [{ key: 'calls', count: '123456' }];
        vi.mocked(api.overview).mockResolvedValueOnce({ coverage: coverage(), summary: first, previous: null });
        const { client, rerender } = mount(api);
        await screen.findAllByText('123,456');
        rerender(<QueryClientProvider client={client}><MetricsDashboard api={api} identity="owner:two" allowed/></QueryClientProvider>);
        expect(screen.queryByText('123,456')).toBeNull();
        await waitFor(() => expect(api.overview).toHaveBeenCalledTimes(2));
        expect(client.getQueriesData({ queryKey: ['model-metrics', location.origin, 'owner:one'] })).toEqual([]);
    });
});
