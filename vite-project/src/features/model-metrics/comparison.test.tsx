import { afterEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { MetricsComparison } from './comparison';
import { apiFixture, coverage, groups, summary } from './test-fixtures';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then((module) => module.reactI18nextMock()));
const clients: QueryClient[] = [];
afterEach(() => { cleanup(); clients.splice(0).forEach((client) => client.clear()); });

it('compares independently selected contracts within the same cohort and marks observed configuration differences', async () => {
    const api = apiFixture(); const client = new QueryClient(); clients.push(client);
    vi.mocked(api.models).mockImplementation(async (query) => {
        const value = groups(); value.groups[0].configurations = [{ provider_id: 'provider-a', model_id: 'model-a', model_name: 'Same name', revisions: [query.contract_revision === '2' ? 'config-2' : 'config-1'] }];
        return value;
    });
    const query = { from: '2026-10-08T00:00:00Z', to: '2026-10-09T00:00:00Z', model_id: 'model-a', purpose: 'agent', origin: 'scheduled_task', contract_revision: '1' };
    render(<QueryClientProvider client={client}><MetricsComparison api={api} query={query} cacheKey={['model-metrics', 'fixture']} accessLost={vi.fn()}/></QueryClientProvider>);
    await screen.findByText('Same single observed revision');
    fireEvent.change(screen.getByRole('textbox', { name: 'B Contract revision' }), { target: { value: '2' } });
    await screen.findByText('Observed versions differ or changed within a window');
    expect(vi.mocked(api.overview).mock.calls.at(-1)![0]).toMatchObject({ ...query, contract_revision: '2' });
    expect(vi.mocked(api.overview).mock.calls[0][0]).toMatchObject({ model_id: 'model-a', purpose: 'agent', origin: 'scheduled_task', contract_revision: '1', from: '2026-10-07T00:00:00.000Z', to: query.from });
    expect(screen.getByText('config-1')).toBeTruthy(); expect(screen.getByText('config-2')).toBeTruthy();
});

it('uses tool cohorts, exposes incomplete coverage and clears the cache when comparison access is revoked', async () => {
    const api = apiFixture(); const client = new QueryClient(); clients.push(client); const accessLost = vi.fn();
    vi.mocked(api.overview).mockResolvedValue({ coverage: coverage('partial'), summary: summary(), previous: null });
    vi.mocked(api.tools).mockResolvedValue(groups('read_file'));
    render(<QueryClientProvider client={client}><MetricsComparison api={api} query={{ from: '2026-10-08T00:00:00Z', to: '2026-10-09T00:00:00Z', tool: 'read_file' }} cacheKey={['model-metrics', 'tool-fixture']} accessLost={accessLost}/></QueryClientProvider>);
    await waitFor(() => expect(api.tools).toHaveBeenCalled());
    expect(api.models).not.toHaveBeenCalled();
    expect(vi.mocked(api.tools).mock.calls[0][0].tool).toBe('read_file');
    expect(screen.getAllByText(/Some configuration versions were not recorded/).length).toBeGreaterThan(0);
    vi.mocked(api.overview).mockRejectedValue({ fixtureAccessDenied: true });
    fireEvent.change(screen.getByRole('textbox', { name: 'B Contract revision' }), { target: { value: 'denied' } });
    await waitFor(() => expect(accessLost).toHaveBeenCalled());
    expect(client.getQueriesData({ queryKey: ['model-metrics', 'tool-fixture'] })).toEqual([]);
});

it('does not substitute a default range when a comparison bound is missing', () => {
    const api = apiFixture(); const client = new QueryClient(); clients.push(client);
    render(<QueryClientProvider client={client}><MetricsComparison api={api} query={{ from: '2026-10-08T00:00:00Z' }} cacheKey={['model-metrics', 'invalid-fixture']} accessLost={vi.fn()}/></QueryClientProvider>);
    expect(api.overview).not.toHaveBeenCalled(); expect(api.models).not.toHaveBeenCalled();
    expect(screen.getAllByRole('alert')).toHaveLength(2);
});
