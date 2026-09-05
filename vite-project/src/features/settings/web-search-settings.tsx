import { getWebSearch, updateWebSearch, testWebSearch } from '@/services/clients';
import { WebSearchForm, type WebSearchSettingsApi } from './web-search-form';

const api: WebSearchSettingsApi = {
    async load() { const result = await getWebSearch(); if (!result.success || !result.data) throw new Error('Web Search unavailable'); return result.data; },
    async save(update) { const result = await updateWebSearch(update); if (!result.success || !result.data) throw new Error('Web Search save failed'); return result.data; },
    async test(update) { const result = await testWebSearch(update); if (!result.success || !result.data) throw new Error('Web Search test failed'); return result.data; },
};

export function WebSearchSettings() { return <WebSearchForm api={api} />; }
