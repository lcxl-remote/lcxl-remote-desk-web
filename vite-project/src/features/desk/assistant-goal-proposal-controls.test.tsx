import { fireEvent, render, screen } from '@testing-library/react';
import { expect, it, vi } from 'vitest';
import { AssistantGoalProposalControls } from './assistant-goal-proposal-controls';
vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));

it('shows AI review while enabled and restores manual controls when disabled', () => {
    const onDecide = vi.fn();
    const { rerender } = render(<AssistantGoalProposalControls automaticApproval revision={false} busy={false} canApprove onDecide={onDecide} />);
    expect(screen.getByRole('status')).toBeInTheDocument();
    expect(screen.queryByRole('button')).toBeNull();
    rerender(<AssistantGoalProposalControls automaticApproval={false} revision={false} busy={false} canApprove onDecide={onDecide} />);
    const buttons = screen.getAllByRole('button');
    fireEvent.click(buttons[0]);
    fireEvent.click(buttons[1]);
    expect(onDecide.mock.calls).toEqual([[true], [false]]);
});

it('does not allow a manual decision on an expired or submitting revision', () => {
    render(<AssistantGoalProposalControls automaticApproval={false} revision busy canApprove onDecide={vi.fn()} />);
    for (const button of screen.getAllByRole('button')) expect(button).toBeDisabled();
});
