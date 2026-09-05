import { describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { WebSearchForm } from './web-search-form';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then((m) => m.reactI18nextMock()));

const config = {
    revision: 0, provider: 'duck_duck_go' as const, has_api_key: false, configured: true,
    providers: [
        { provider: 'duck_duck_go' as const, display_name: 'DuckDuckGo', requires_api_key: false },
        { provider: 'brave' as const, display_name: 'Brave', requires_api_key: true },
        { provider: 'tavily' as const, display_name: 'Tavily', requires_api_key: true },
    ],
};

function createApi() {
    return { load: vi.fn().mockResolvedValue(config), save: vi.fn().mockResolvedValue({ ...config, revision: 1 }), test: vi.fn().mockResolvedValue({ provider: 'duck_duck_go', result_count: 1, latency_ms: 42 }) };
}

describe('shared Web Search settings', () => {
    it('loads a keyless default without making a search request', async () => {
        const api = createApi();
        render(<WebSearchForm api={api} />);
        await screen.findByRole('combobox', { name: 'Search provider' });
        expect(screen.queryByLabelText('API Key')).not.toBeInTheDocument();
        expect(api.test).not.toHaveBeenCalled();
        fireEvent.click(screen.getByRole('button', { name: 'Save' }));
        await waitFor(() => expect(api.save).toHaveBeenCalledWith({ expected_revision: 0, provider: 'duck_duck_go', api_key: '' }));
        expect(api.test).not.toHaveBeenCalled();
    });

    it('clears unsaved secrets when switching providers', async () => {
        const api = createApi();
        render(<WebSearchForm api={api} />);
        const provider = await screen.findByRole('combobox', { name: 'Search provider' });
        fireEvent.change(provider, { target: { value: 'brave' } });
        fireEvent.change(screen.getByLabelText('API Key'), { target: { value: 'brave-secret' } });
        fireEvent.change(provider, { target: { value: 'tavily' } });
        expect(screen.getByLabelText('API Key')).toHaveValue('');
        expect(screen.getByRole('button', { name: 'Save' })).toBeDisabled();
        fireEvent.change(provider, { target: { value: 'duck_duck_go' } });
        fireEvent.click(screen.getByRole('button', { name: 'Save' }));
        await waitFor(() => expect(api.save).toHaveBeenCalledWith({ expected_revision: 0, provider: 'duck_duck_go', api_key: '' }));
    });

    it('tests only explicitly without saving and prevents repeated clicks', async () => {
        const api = createApi();
        let finish!: (value: unknown) => void;
        api.test.mockImplementation(() => new Promise((resolve) => { finish = resolve; }));
        render(<WebSearchForm api={api} />);
        await screen.findByRole('combobox', { name: 'Search provider' });
        fireEvent.click(screen.getByRole('button', { name: 'Test connection' }));
        fireEvent.click(screen.getByRole('button', { name: 'Test connection' }));
        expect(api.test).toHaveBeenCalledTimes(1);
        expect(api.save).not.toHaveBeenCalled();
        finish({ provider: 'duck_duck_go', result_count: 1, latency_ms: 42 });
        await screen.findByText('Connection test passed: 1 results in 42 ms.');
    });

    it('requires reload after an uncertain save without automatic retry', async () => {
        const api = createApi(); api.save.mockRejectedValue(new Error('conflict'));
        render(<WebSearchForm api={api} />);
        await screen.findByRole('combobox', { name: 'Search provider' });
        fireEvent.click(screen.getByRole('button', { name: 'Save' }));
        await waitFor(() => expect(screen.queryByRole('combobox')).not.toBeInTheDocument());
        expect(api.save).toHaveBeenCalledTimes(1);
        expect(api.load).toHaveBeenCalledTimes(1);
    });

    it('does not offer an editable default when configuration cannot be loaded', async () => {
        const api = createApi(); api.load.mockRejectedValue(new Error('denied'));
        render(<WebSearchForm api={api} />);
        await screen.findByText('Could not load settings. Please retry.');
        expect(screen.queryByRole('button', { name: 'Save' })).not.toBeInTheDocument();
        expect(api.test).not.toHaveBeenCalled();
    });
});
