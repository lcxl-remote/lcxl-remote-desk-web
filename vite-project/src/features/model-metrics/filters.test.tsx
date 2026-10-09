import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { MetricsFilters } from './filters';
import { MetricsCoverage } from './coverage';
import { coverage, groups } from './test-fixtures';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then((module) => module.reactI18nextMock()));
afterEach(cleanup);

describe('model metric filters and coverage', () => {
    it('selects original stable identity when two providers use the same model name', () => {
        const choices = groups();
        choices.groups.push({ ...choices.groups[0], key: 'provider-b:model-b', provider_id: 'provider-b', model_id: 'model-b' });
        const change = vi.fn();
        render(<MetricsFilters value={{ origin: 'subagent' }} change={change} models={choices} loading={false} failed={false}/>);
        fireEvent.change(screen.getByRole('combobox', { name: 'Model' }), { target: { value: JSON.stringify(['provider-b', 'model-b']) } });
        expect(change).toHaveBeenLastCalledWith({ origin: 'subagent', provider_id: 'provider-b', model_id: 'model-b' });
    });

    it('preserves an unlisted historical ID and exposes exact lookup after choices fail', () => {
        const change = vi.fn();
        render(<MetricsFilters value={{ provider_id: 'deleted-provider', model_id: 'deleted-model' }} change={change} loading={false} failed/>);
        expect((screen.getByRole('combobox', { name: 'Model' }) as HTMLSelectElement).value).toBe(JSON.stringify(['deleted-provider', 'deleted-model']));
        fireEvent.change(screen.getByRole('combobox', { name: 'Purpose' }), { target: { value: 'context_compression' } });
        expect(change).toHaveBeenLastCalledWith({ provider_id: 'deleted-provider', model_id: 'deleted-model', purpose: 'context_compression' });
        expect(screen.getByText('Choices are unavailable. Exact IDs or tool names remain usable.')).toBeTruthy();
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
