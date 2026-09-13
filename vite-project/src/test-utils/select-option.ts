import { fireEvent, screen } from '@testing-library/react';

/** Operate the actual Radix trigger and option, rather than a hidden native select. */
export async function selectOption(trigger: HTMLElement, name: string | RegExp) {
    fireEvent.keyDown(trigger, { key: 'ArrowDown' });
    fireEvent.click(await screen.findByRole('option', { name }));
}
