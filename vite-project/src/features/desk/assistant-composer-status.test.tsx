import { cleanup, fireEvent, render, screen, within } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';
import { AssistantComposerStatus } from './assistant-composer-status';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(module => module.reactI18nextMock()));
afterEach(cleanup);

it('shows approval alongside selected context and opens settings without sending a message', () => {
    const onApprovalSettings = vi.fn();
    const onContext = vi.fn();
    const onClearGoal = vi.fn();
    const onSubmit = vi.fn();
    const view = render(<form onSubmit={event => { event.preventDefault(); onSubmit(); }}>
        <AssistantComposerStatus automaticApproval onApprovalSettings={onApprovalSettings}
            contextCount={23} onContext={onContext} startGoal onClearGoal={onClearGoal} />
    </form>);
    const status = screen.getByTestId('assistant-composer-status');
    const context = within(status).getByRole('button', { name: '23 context items selected' });
    const goal = within(status).getByRole('button', { name: 'Continue working' });
    expect(context.querySelector('svg')).toHaveClass('lucide-layers');
    expect(goal.querySelector('svg')).toHaveClass('lucide-toggle-right');
    const approval = within(status).getByRole('button', { name: 'AI approval is on' });
    expect(approval).toHaveAttribute('aria-haspopup', 'dialog');
    expect(approval.querySelector('svg')).toHaveClass('lucide-bot');
    for (const button of [context, goal, approval]) {
        const label = button.querySelector<HTMLElement>('span.assistant-action-label')!;
        label.style.display = 'none';
        expect(button).toHaveAccessibleName(button.getAttribute('aria-label')!);
        expect(button.title).toBe(button.getAttribute('aria-label'));
        fireEvent.click(button);
    }
    expect(onApprovalSettings).toHaveBeenCalledOnce();
    expect(onContext).toHaveBeenCalledOnce();
    expect(onClearGoal).toHaveBeenCalledOnce();
    expect(onSubmit).not.toHaveBeenCalled();
    view.rerender(<AssistantComposerStatus automaticApproval={false} onApprovalSettings={onApprovalSettings}
        contextCount={23} onContext={onContext} />);
    expect(screen.queryByText('AI approval is on')).toBeNull();
    expect(screen.getByRole('button', { name: '23 context items selected' })).toBeVisible();
    expect(screen.queryByRole('button', { name: 'Continue working' })).toBeNull();
    view.rerender(<AssistantComposerStatus automaticApproval={false} contextCount={0} onContext={onContext} />);
    expect(screen.queryByRole('button')).toBeNull();
});

it('shows an active approval status when settings cannot be opened', () => {
    render(<AssistantComposerStatus automaticApproval />);
    const status = screen.getByRole('status', { name: 'AI approval is on' });
    status.querySelector<HTMLElement>('span.assistant-action-label')!.style.display = 'none';
    expect(status).toHaveAccessibleName('AI approval is on');
    expect(status.title).toBe('AI approval is on');
    expect(screen.queryByRole('button')).toBeNull();
});
