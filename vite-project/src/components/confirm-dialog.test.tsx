import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';
import { useConfirm } from './confirm-dialog';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
afterEach(cleanup);

let confirmation: ReturnType<typeof useConfirm>;
function Harness() {
    confirmation = useConfirm();
    return confirmation.dialog;
}

it('requires explicit acceptance and lets cancellation and Escape decline', async () => {
    render(<Harness />);
    let result!: Promise<boolean>;
    act(() => { result = confirmation.confirm({ title: 'Delete attachment?' }); });
    expect(screen.getByRole('alertdialog', { name: 'Delete attachment?' })).toBeVisible();
    fireEvent.click(screen.getByRole('button', { name: 'common.cancel' }));
    expect(await result).toBe(false);

    act(() => { result = confirmation.confirm({ title: 'Delete attachment?' }); });
    fireEvent.keyDown(screen.getByRole('alertdialog'), { key: 'Escape' });
    expect(await result).toBe(false);

    act(() => { result = confirmation.confirm({ title: 'Delete attachment?' }); });
    fireEvent.click(screen.getByRole('button', { name: 'common.confirm' }));
    expect(await result).toBe(true);
});

it('declines a second request without replacing the pending action', async () => {
    render(<Harness />);
    let first!: Promise<boolean>;
    act(() => { first = confirmation.confirm({ title: 'First action' }); });
    expect(await confirmation.confirm({ title: 'Second action' })).toBe(false);
    expect(screen.getByRole('alertdialog', { name: 'First action' })).toBeVisible();
    fireEvent.click(screen.getByRole('button', { name: 'common.confirm' }));
    expect(await first).toBe(true);
});

it('declines an unresolved confirmation when its page unmounts', async () => {
    const view = render(<Harness />);
    let result!: Promise<boolean>;
    act(() => { result = confirmation.confirm({ title: 'Old page action' }); });
    view.unmount();
    expect(await result).toBe(false);
    expect(await confirmation.confirm({ title: 'Late response from the old page' })).toBe(false);
});

it('returns keyboard focus to the invoking control after cancellation', async () => {
    function Page() {
        const { confirm, dialog } = useConfirm();
        return <>{dialog}<button onClick={() => void confirm({ title: 'Confirm action' })}>Open</button></>;
    }
    render(<Page />);
    const trigger = screen.getByRole('button', { name: 'Open' });
    trigger.focus();
    fireEvent.click(trigger);
    fireEvent.click(screen.getByRole('button', { name: 'common.cancel' }));
    await waitFor(() => expect(trigger).toHaveFocus());
});
