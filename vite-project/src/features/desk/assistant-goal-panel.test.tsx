import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AssistantGoalPanel, goalPlanningAction, goalProgressMessage } from './assistant-goal-panel';
import type { AiAssistantGoal } from './use-ai-assistant-chat';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));

const goal = { goalId: 'goal', state: 'running', goalText: '打开计算器并计算 6+8', checkpointSummary: '已输入 6+8' } as AiAssistantGoal;
const props = { connected: true, main: true, enabled: true, busy: false,
    onDetails: vi.fn(), onAction: vi.fn(), onReply: vi.fn(), onContinue: vi.fn() };

describe('AssistantGoalPanel', () => {
    it('keeps goal and progress visible as the server state changes', () => {
        const { rerender } = render(<AssistantGoalPanel {...props} goal={goal} />);
        expect(screen.getByText(goal.goalText)).toBeTruthy();
        expect(screen.getByText('Working')).toBeTruthy();
        expect(screen.getByText('已输入 6+8')).toBeTruthy();
        rerender(<AssistantGoalPanel {...props} goal={{ ...goal, state: 'completed', checkpointSummary: '6+8=14' }} />);
        expect(screen.getByText('Completed')).toBeTruthy();
        expect(screen.getByText('6+8=14')).toBeTruthy();
        expect(screen.queryByRole('button', { name: 'Cancel goal' })).toBeNull();
    });

    it('offers reply for a concrete question without resuming the goal', () => {
        render(<AssistantGoalPanel {...props} goal={{ ...goal, state: 'waiting_user', statusReason: '保存到哪里？' }} />);
        expect(screen.getByText('保存到哪里？')).toBeTruthy();
        expect(screen.queryByRole('button', { name: 'Resume goal' })).toBeNull();
        fireEvent.click(screen.getByRole('button', { name: 'Reply to question' }));
        expect(props.onReply).toHaveBeenCalled();
    });

    it('keeps the main goal on child tabs and displays the last known state offline', () => {
        render(<AssistantGoalPanel {...props} main={false} connected={false} goal={goal} />);
        expect(screen.getByText('Main goal')).toBeTruthy();
        expect(screen.getByText('Last known state · disconnected')).toBeTruthy();
        expect(screen.getAllByRole('button')).toHaveLength(1);
        fireEvent.click(screen.getByRole('button', { name: 'Details' }));
        expect(props.onDetails).toHaveBeenCalled();
    });

    it('maps stalled retries, terminal states and hides internal waiting references', () => {
        expect(goalPlanningAction({ state: 'paused', pauseReason: 'stalled' })).toBe('retry_stalled');
        expect(goalPlanningAction({ state: 'paused', pauseReason: 'owner' })).toBe('resume');
        for (const state of ['completed', 'failed', 'cancelled', 'waiting_user', 'future_state']) {
            expect(goalPlanningAction({ state })).toBeNull();
        }
        expect(goalProgressMessage({ state: 'waiting_approval', checkpointSummary: 'Waiting for request-id' })).toBeNull();
    });
});
