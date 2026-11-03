import { afterEach, describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { MemoryRouter } from 'react-router-dom';
import { axiosInstance } from '@/lib/kubb-client';
import { deskErrorCodeEnum } from '@/services/types';
import DeskList from './desk-list';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
vi.mock('./host-readiness-banners', () => ({ HostReadinessBanners: () => null }));
const originalAdapter = axiosInstance.defaults.adapter;
let query: QueryClient;
afterEach(() => { axiosInstance.defaults.adapter = originalAdapter; query?.clear(); });

function show(body: unknown) {
    axiosInstance.defaults.adapter = async config => ({ data: body, config, status: 200, statusText: 'OK', headers: {} });
    query = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: 0 } } });
    render(<QueryClientProvider client={query}><MemoryRouter><DeskList /></MemoryRouter></QueryClientProvider>);
}

describe('real connection-list client contract', () => {
    it('shows a backend failure instead of an empty device list for HTTP 200 / 109', async () => {
        show({ success: false, code: deskErrorCodeEnum.SHARED_STORE_UNAVAILABLE, message: 'Presence unavailable', data: null });
        expect(await screen.findByRole('alert')).toHaveTextContent('Could not load devices. Try again.');
        expect(screen.queryByText('No connections found')).not.toBeInTheDocument();
        expect(screen.getByRole('button', { name: 'Refresh' })).toBeEnabled();
    });

    it('reads the success envelope and displays actual empty data', async () => {
        show({ success: true, code: deskErrorCodeEnum.SUCCESS, data: [] });
        expect(await screen.findByText('No connections found')).toBeInTheDocument();
        expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    });
});
