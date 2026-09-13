import { useState } from 'react';
import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { Disclosure } from './disclosure';
import { Input } from './input';
import { SelectField } from './select-field';
import { SelectItem } from './select';
import { selectOption } from '@/test-utils/select-option';

function Form() {
    const [value, setValue] = useState('one');
    return <SelectField aria-label="Filter" value={value} onValueChange={setValue}>
        <SelectItem value="__empty__">All</SelectItem>
        <SelectItem value="one">One</SelectItem>
        <SelectItem value="two">Two</SelectItem>
    </SelectField>;
}

describe('shared field interactions', () => {
    it('opens through the keyboard and supports choosing and clearing a filter', async () => {
        render(<Form />);
        await selectOption(screen.getByRole('combobox'), 'Two');
        expect(screen.getByRole('combobox')).toHaveTextContent('Two');
        await selectOption(screen.getByRole('combobox'), 'All');
        expect(screen.getByRole('combobox')).toHaveTextContent('All');
    });
    it('disables the actual trigger and does not change the value', () => {
        const change = vi.fn();
        render(<SelectField aria-label="Filter" value="one" disabled onValueChange={change}><SelectItem value="one">One</SelectItem></SelectField>);
        expect(screen.getByRole('combobox')).toBeDisabled();
        fireEvent.click(screen.getByRole('combobox'));
        expect(change).not.toHaveBeenCalled();
        expect(screen.queryByRole('listbox')).not.toBeInTheDocument();
    });
    it('keeps edited content and nested expansion when collapsed and never submits a form', () => {
        const submit = vi.fn((event: React.FormEvent) => event.preventDefault());
        render(<form onSubmit={submit}><Disclosure title="Advanced"><Input aria-label="Note" defaultValue="" /><Disclosure title="More">Nested content</Disclosure></Disclosure></form>);
        const trigger = screen.getByRole('button', { name: 'Advanced' });
        expect(trigger).toHaveAttribute('aria-expanded', 'false');
        fireEvent.click(trigger);
        fireEvent.change(screen.getByLabelText('Note'), { target: { value: 'Keep this' } });
        fireEvent.click(screen.getByRole('button', { name: 'More' }));
        fireEvent.click(trigger);
        fireEvent.click(trigger);
        expect(screen.getByLabelText('Note')).toHaveValue('Keep this');
        expect(screen.getByRole('button', { name: 'More' })).toHaveAttribute('aria-expanded', 'true');
        expect(submit).not.toHaveBeenCalled();
    });
});
