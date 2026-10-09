import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { deskErrorCodeEnum } from '@/services/types';
import { MetricsCollectionSettings } from './settings';
import { apiFixture } from './test-fixtures';
import type { MetricsApi } from './dashboard';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then((module) => module.reactI18nextMock()));
const clients: QueryClient[] = [];
afterEach(() => { cleanup(); clients.splice(0).forEach((client) => client.clear()); vi.restoreAllMocks(); });
const initialSettings = { revision: '1', enabled: true, detail_days: 7, five_minute_days: 7, hourly_days: 90, mutable_days: 7, detail_row_budget: 100000, event_row_budget: 20000, compact_row_budget: 100000, rollup_row_budget: 25000, series_per_bucket: 256, storage_budget_bytes: '268435456' };
function mount(api: MetricsApi, allowed = true) {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    clients.push(client);
    return { client, ...render(<QueryClientProvider client={client}><MetricsCollectionSettings api={api} identity="owner:one" allowed={allowed}/></QueryClientProvider>) };
}

describe('model statistics settings', () => {
    it('keeps an edited draft after a revision conflict and reloads only on explicit action', async () => {
        const api = apiFixture();
        const settings = { revision: '1', enabled: true, detail_days: 7, five_minute_days: 7, hourly_days: 90, mutable_days: 7, detail_row_budget: 100000, event_row_budget: 20000, compact_row_budget: 100000, rollup_row_budget: 25000, series_per_bucket: 256, storage_budget_bytes: '268435456' };
        vi.mocked(api.settings).mockResolvedValueOnce(settings).mockResolvedValue({ ...settings, revision: '2', detail_days: 6 });
        vi.mocked(api.save).mockRejectedValue(Object.assign(new Error('Changed elsewhere'), { code: deskErrorCodeEnum.REVISION_CONFLICT }));
        mount(api);
        const days = await screen.findByRole('spinbutton', { name: 'Detail retention days' });
        fireEvent.change(days, { target: { value: '8' } });
        fireEvent.submit(days.closest('form')!);
        await screen.findByText('Settings changed elsewhere. Load and review the current revision before saving; this draft cannot overwrite it.');
        expect((days as HTMLInputElement).value).toBe('8');
        expect((screen.getByRole('button', { name: 'Save' }) as HTMLButtonElement).disabled).toBe(true);
        expect(api.settings).toHaveBeenCalledTimes(1);
        fireEvent.click(screen.getByRole('button', { name: 'Load current settings' }));
        await waitFor(() => expect((screen.getByRole('spinbutton', { name: 'Detail retention days' }) as HTMLInputElement).value).toBe('6'));
        expect((screen.getByRole('button', { name: 'Save' }) as HTMLButtonElement).disabled).toBe(false);
    });

    it('does not read settings without owner or administrator access', () => {
        const api = apiFixture();
        mount(api, false);
        expect(api.settings).not.toHaveBeenCalled();
        expect(api.save).not.toHaveBeenCalled();
        expect(screen.getByRole('alert')).toBeTruthy();
    });

    it('clears settings from the old identity before loading another owner', async () => {
        const api = apiFixture();
        vi.mocked(api.settings).mockResolvedValueOnce(initialSettings).mockResolvedValue({ ...initialSettings, revision: '2', detail_days: 5 });
        const view = mount(api);
        await screen.findByRole('spinbutton', { name: 'Detail retention days' });
        view.rerender(<QueryClientProvider client={view.client}><MetricsCollectionSettings api={api} identity="owner:two" allowed/></QueryClientProvider>);
        await waitFor(() => expect((screen.getByRole('spinbutton', { name: 'Detail retention days' }) as HTMLInputElement).value).toBe('5'));
        expect(view.client.getQueriesData({ queryKey: ['model-metrics-settings', location.origin, 'owner:one'] })).toEqual([]);
    });

    it('saves collection preferences without fetching model call statistics', async () => {
        const api = apiFixture();
        vi.mocked(api.settings).mockResolvedValue(initialSettings);
        vi.mocked(api.save).mockImplementation(async (settings) => ({ ...settings, revision: '2' }));
        mount(api);
        const enabled = await screen.findByRole('checkbox', { name: 'Collect model call statistics' });
        fireEvent.click(enabled);
        fireEvent.submit(enabled.closest('form')!);
        await waitFor(() => expect(api.save).toHaveBeenCalled());
        expect(vi.mocked(api.save).mock.calls[0][0]).toMatchObject({ enabled: false, revision: '1' });
        expect(api.overview).not.toHaveBeenCalled();
        expect(api.status).not.toHaveBeenCalled();
    });

    it('hides the form when access is lost during a save', async () => {
        const api = apiFixture();
        const denied = { response: { status: 403 } };
        api.accessError = (error) => error === denied;
        vi.mocked(api.settings).mockResolvedValue(initialSettings);
        vi.mocked(api.save).mockRejectedValue(denied);
        mount(api);
        const enabled = await screen.findByRole('checkbox', { name: 'Collect model call statistics' });
        fireEvent.submit(enabled.closest('form')!);
        await screen.findByRole('alert');
        expect(screen.queryByRole('checkbox')).toBeNull();
    });
});
