import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import { selectOption } from '@/test-utils/select-option';
import { CallFilters } from './call-filters';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then((module) => module.reactI18nextMock()));
afterEach(cleanup);

describe('model metric call filters', () => {
    it('chooses outcomes and latency and clears incompatible conditions when changing record kind', async () => {
        const change = vi.fn();
        render(<CallFilters value={{ min_duration_ms: 500 }} change={change} inferredKind="call"/>);
        await selectOption(screen.getByRole('combobox', { name: 'Outcome' }), 'Returned');
        expect(change).toHaveBeenLastCalledWith({ min_duration_ms: 500, outcome: 'returned' });
        await selectOption(screen.getByRole('combobox', { name: 'Latency measurement' }), 'First content latency');
        expect(change).toHaveBeenLastCalledWith({ min_duration_ms: 500, latency: 'first_content' });
        await selectOption(screen.getByRole('combobox', { name: 'Record type' }), 'Tool');
        expect(change).toHaveBeenLastCalledWith({ record_kind: 'tool' });
    });

    it('clears the outcome or explicit kind and disables the runtime outcome trigger', async () => {
        const change = vi.fn();
        const view = render(<CallFilters value={{ record_kind: 'tool', outcome: 'rejected' }} change={change} inferredKind="call"/>);
        await selectOption(screen.getByRole('combobox', { name: 'Outcome' }), 'All conclusions');
        expect(change).toHaveBeenLastCalledWith({ record_kind: 'tool', outcome: undefined });
        await selectOption(screen.getByRole('combobox', { name: 'Record type' }), 'Automatic from shared filters');
        expect(change).toHaveBeenLastCalledWith({ record_kind: undefined });
        view.rerender(<CallFilters value={{ record_kind: 'runtime' }} change={change} inferredKind="call"/>);
        expect(screen.getByRole('combobox', { name: 'Outcome' })).toBeDisabled();
    });
});
