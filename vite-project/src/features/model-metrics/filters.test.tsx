import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { MetricsActiveFilters, MetricsFilters } from './filters';
import { MetricsCoverage } from './coverage';
import { coverage, groups } from './test-fixtures';
import { selectOption } from '@/test-utils/select-option';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then((module) => module.reactI18nextMock()));
afterEach(cleanup);

describe('model metric filters and coverage', () => {
    it('shows every selected dimension without relying on the model and tool candidate lists', () => {
        const change = vi.fn();
        const value = { provider_id: 'deleted-provider', model_id: 'deleted-model', tool: 'request_permissions', surface: 'assistant', purpose: 'agent', origin: 'subagent', error: 'input.invalid_json', contract_revision: '1.3', include_probe: true, granularity: 'five_minutes', limit: 50 } as const;
        render(<MetricsActiveFilters value={value} change={change}/>);
        expect(screen.getByRole('group', { name: 'Active filters' }).querySelectorAll('button')).toHaveLength(10);
        expect(screen.getByText('Provider ID: deleted-provider')).toBeTruthy();
        expect(screen.getByText('Model ID: deleted-model')).toBeTruthy();
        expect(screen.getByText('Tool: request_permissions')).toBeTruthy();
        fireEvent.click(screen.getByRole('button', { name: 'Remove filter: Tool: request_permissions' }));
        expect(change).toHaveBeenLastCalledWith({ ...value, tool: undefined });
        fireEvent.click(screen.getByRole('button', { name: 'Clear filters' }));
        expect(change).toHaveBeenLastCalledWith({ granularity: 'five_minutes', limit: 50 });
    });

    it('does not treat default display settings or excluded probes as active filters', () => {
        const view = render(<MetricsActiveFilters value={{ granularity: 'hour', limit: 50, include_probe: false }} change={vi.fn()}/>);
        expect(screen.queryByRole('group', { name: 'Active filters' })).toBeNull();
        const change = vi.fn();
        view.rerender(<MetricsActiveFilters value={{ granularity: 'hour', limit: 50, include_probe: true }} change={change}/>);
        const group = screen.getByRole('group', { name: 'Active filters' });
        fireEvent.click(group.querySelector('button')!);
        expect(change).toHaveBeenLastCalledWith({ granularity: 'hour', limit: 50, include_probe: undefined });
    });

    it('selects original stable identity when two providers use the same model name', async () => {
        const choices = groups();
        choices.groups.push({ ...choices.groups[0], key: 'provider-b:model-b', provider_id: 'provider-b', model_id: 'model-b' });
        const change = vi.fn();
        const view = render(<MetricsFilters value={{ origin: 'subagent' }} change={change} models={choices} loading={false} failed={false}/>);
        await selectOption(screen.getByRole('combobox', { name: 'Model' }), 'Same name · provider-b / model-b');
        expect(change).toHaveBeenLastCalledWith({ origin: 'subagent', provider_id: 'provider-b', model_id: 'model-b' });
        view.rerender(<MetricsFilters value={{ origin: 'subagent', provider_id: 'provider-b', model_id: 'model-b' }} change={change} models={choices} loading={false} failed={false}/>);
        await selectOption(screen.getByRole('combobox', { name: 'Model' }), 'All categories');
        expect(change).toHaveBeenLastCalledWith({ origin: 'subagent', provider_id: undefined, model_id: undefined });
    });

    it('preserves an unlisted historical ID and exposes exact lookup after choices fail', async () => {
        const change = vi.fn();
        render(<MetricsFilters value={{ provider_id: 'deleted-provider', model_id: 'deleted-model' }} change={change} loading={false} failed/>);
        expect(screen.getByRole('combobox', { name: 'Model' })).toHaveTextContent('Current exact model ID (outside the list)');
        await selectOption(screen.getByRole('combobox', { name: 'Purpose' }), 'Context compression');
        expect(change).toHaveBeenLastCalledWith({ provider_id: 'deleted-provider', model_id: 'deleted-model', purpose: 'context_compression' });
        expect(screen.getByText('Choices are unavailable. Exact IDs or tool names remain usable.')).toBeTruthy();
    });

    it('clears an unlisted tool through the themed menu and toggles probes through the themed checkbox', async () => {
        const change = vi.fn();
        const value = { tool: '__empty__', origin: 'subagent' } as const;
        render(<MetricsFilters value={value} change={change} loading={false} failed={false}/>);
        expect(screen.getByRole('combobox', { name: 'Tool' })).toHaveTextContent('__empty__');
        await selectOption(screen.getByRole('combobox', { name: 'Tool' }), 'All categories');
        expect(change).toHaveBeenLastCalledWith({ origin: 'subagent', tool: undefined });
        fireEvent.click(screen.getByRole('checkbox', { name: 'Include provider probes' }));
        expect(change).toHaveBeenLastCalledWith({ ...value, include_probe: true });
    });

    it('distinguishes a pre-activation interval from an interval with no matching samples', () => {
        const view = render(<MetricsCoverage value={coverage('not_collected')} empty/>);
        expect(screen.getByText('Statistics were not enabled during this period. No data is available.')).toBeTruthy();
        expect(screen.queryByText('No records match this period and its filters, so an error rate cannot be calculated.')).toBeNull();
        view.rerender(<MetricsCoverage value={coverage()} empty/>);
        expect(screen.getByText('No records match this period and its filters, so an error rate cannot be calculated.')).toBeTruthy();
        expect(screen.queryByText('Statistics were not enabled during this period. No data is available.')).toBeNull();
    });
});
