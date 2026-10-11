import { useState } from 'react';
import { ListTree } from 'lucide-react';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { AssistantComposerMenu } from './assistant-composer-menu';

const viewport = vi.hoisted(() => ({ mobile: false }));
vi.mock('@/hooks/use-mobile', () => ({ useIsMobile: () => viewport.mobile }));
vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(module => module.reactI18nextMock()));
beforeEach(() => { viewport.mobile = false; });
afterEach(() => { cleanup(); vi.clearAllMocks(); });

function Fixture({ onAction, onContext, disabled = false }: {
    onAction: (action: string) => void; onContext: () => void; disabled?: boolean;
}) {
    const [checked, setChecked] = useState(false);
    return <AssistantComposerMenu onContext={onContext} goal={{ checked, disabled, onCheckedChange: setChecked,
        summary: checked ? <p>Goal budget</p> : undefined }} sections={[
        { label: 'Resources', actions: [{ label: 'Attachments', disabled, onSelect: () => onAction('attachments') }] },
        { label: 'Manage', actions: [{ label: 'AI approval', onSelect: () => onAction('approval') }] },
        { label: 'Details', actions: [{ label: 'Task details', icon: ListTree, onSelect: () => onAction('details') }] },
    ]} />;
}

async function openMenu() {
    const trigger = screen.getByRole('button', { name: 'Conversation actions' });
    if (viewport.mobile) fireEvent.click(trigger);
    else fireEvent.keyDown(trigger, { key: 'Enter' });
    return screen.findByRole(viewport.mobile ? 'dialog' : 'menu');
}

describe.each([false, true])('composer + menu on mobile=%s', mobile => {
    beforeEach(() => { viewport.mobile = mobile; });

    it.each(['Attachments', 'AI approval', 'Task details'] as const)('opens %s without submitting the composer and closes the menu', async label => {
        const onAction = vi.fn();
        const onSubmit = vi.fn();
        render(<form onSubmit={event => { event.preventDefault(); onSubmit(); }}>
            <Fixture onAction={onAction} onContext={vi.fn()} />
        </form>);
        await openMenu();
        fireEvent.click(screen.getByRole(mobile ? 'button' : 'menuitem', { name: label }));
        expect(onAction).toHaveBeenCalledExactlyOnceWith({ Attachments: 'attachments', 'AI approval': 'approval', 'Task details': 'details' }[label]);
        expect(onSubmit).not.toHaveBeenCalled();
        await waitFor(() => expect(screen.queryByRole(mobile ? 'dialog' : 'menu')).toBeNull());
    });

    it('keeps context selection available alongside conversation management', async () => {
        const onContext = vi.fn();
        render(<Fixture onAction={vi.fn()} onContext={onContext} />);
        await openMenu();
        expect(screen.getByRole(mobile ? 'button' : 'menuitem', { name: 'Add context' }).querySelector('svg')).toHaveClass('lucide-layers');
        expect(screen.getByRole(mobile ? 'button' : 'menuitem', { name: 'Task details' }).querySelector('svg')).toHaveClass('lucide-list-tree');
        fireEvent.click(screen.getByRole(mobile ? 'button' : 'menuitem', { name: 'Add context' }));
        expect(onContext).toHaveBeenCalledOnce();
        await waitFor(() => expect(screen.queryByRole(mobile ? 'dialog' : 'menu')).toBeNull());
    });

    it('keeps disabled actions and goal selection unavailable', async () => {
        const onAction = vi.fn();
        render(<Fixture onAction={onAction} onContext={vi.fn()} disabled />);
        await openMenu();
        const attachment = screen.getByRole(mobile ? 'button' : 'menuitem', { name: 'Attachments' });
        const goal = screen.getByRole(mobile ? 'button' : 'menuitemcheckbox', { name: 'Continue working' });
        if (mobile) {
            expect(attachment).toBeDisabled(); expect(goal).toBeDisabled();
        } else {
            expect(attachment).toHaveAttribute('aria-disabled', 'true'); expect(goal).toHaveAttribute('aria-disabled', 'true');
        }
        fireEvent.click(attachment); fireEvent.click(goal);
        expect(onAction).not.toHaveBeenCalled();
        expect(screen.queryByText('Goal budget')).toBeNull();
    });

    it('retains the goal toggle and budget summary', async () => {
        render(<Fixture onAction={vi.fn()} onContext={vi.fn()} />);
        await openMenu();
        const inactiveGoal = screen.getByRole(mobile ? 'button' : 'menuitemcheckbox', { name: 'Continue working' });
        expect(inactiveGoal.querySelector('svg')).toHaveClass('lucide-toggle-left');
        expect(inactiveGoal.querySelector('.lucide-check')).toBeNull();
        fireEvent.click(inactiveGoal);
        if (!mobile) { await waitFor(() => expect(screen.queryByRole('menu')).toBeNull()); await openMenu(); }
        expect(screen.getByRole(mobile ? 'button' : 'menuitemcheckbox', { name: 'Continue working' }))
            .toHaveAttribute(mobile ? 'aria-pressed' : 'aria-checked', 'true');
        expect(screen.getByText('Goal budget')).toBeVisible();
        const enabledGoal = screen.getByRole(mobile ? 'button' : 'menuitemcheckbox', { name: 'Continue working' });
        expect(enabledGoal.querySelector('svg')).toHaveClass('lucide-toggle-right');
        expect(enabledGoal.querySelector('.lucide-check')).toBeNull();
        if (mobile) fireEvent.click(enabledGoal);
        else fireEvent.keyDown(enabledGoal, { key: ' ' });
        if (!mobile) { await waitFor(() => expect(screen.queryByRole('menu')).toBeNull()); await openMenu(); }
        const resetGoal = screen.getByRole(mobile ? 'button' : 'menuitemcheckbox', { name: 'Continue working' });
        expect(resetGoal).toHaveAttribute(mobile ? 'aria-pressed' : 'aria-checked', 'false');
        expect(resetGoal.querySelector('svg')).toHaveClass('lucide-toggle-left');
        expect(screen.queryByText('Goal budget')).toBeNull();
    });
});
