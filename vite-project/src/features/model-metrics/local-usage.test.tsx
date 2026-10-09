import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { getModelUsage } from '@/services/clients';
import { deskErrorCodeEnum } from '@/services/types';
import { LocalModelUsage } from './local-usage';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then((module) => module.reactI18nextMock()));
vi.mock('@/services/clients', () => ({ getModelUsage: vi.fn() }));
vi.mock('@/features/usage/model-usage-chart', () => ({ ModelUsageChart: () => <div data-testid="usage-chart"/> }));
const clients: QueryClient[] = [];
afterEach(() => { cleanup(); clients.splice(0).forEach((client) => client.clear()); vi.clearAllMocks(); });

describe('local model metering collection boundary', () => {
    it('distinguishes pre-activation time from a retained range without samples', async () => {
        const client = new QueryClient(); clients.push(client);
        const result = { items: [], range: { from: '2026-10-05T00:00:00Z', to: '2026-10-06T00:00:00Z', granularity: 'hour' }, usage_source: 'observations', available_from: '2026-10-07T00:00:00Z', partial: false };
        vi.mocked(getModelUsage).mockResolvedValueOnce({ success: true, code: deskErrorCodeEnum.SUCCESS, message: null, data: result });
        const view = render(<QueryClientProvider client={client}><LocalModelUsage identity="owner:one"/></QueryClientProvider>);
        await screen.findByText('Statistics were not enabled during this period. No data is available.');
        expect(screen.queryByTestId('usage-chart')).toBeNull();
        vi.mocked(getModelUsage).mockResolvedValue({ success: true, code: deskErrorCodeEnum.SUCCESS, message: null, data: { ...result, range: { ...result.range, from: '2026-10-08T00:00:00Z', to: '2026-10-09T00:00:00Z' } } });
        view.rerender(<QueryClientProvider client={client}><LocalModelUsage identity="owner:two"/></QueryClientProvider>);
        await screen.findByText('No records match this period and its filters, so an error rate cannot be calculated.');
        expect(screen.queryByText('Statistics were not enabled during this period. No data is available.')).toBeNull();
        await waitFor(() => expect(client.getQueriesData({ queryKey: ['local-model-usage', location.origin, 'owner:one'] })).toEqual([]));
    });
});
