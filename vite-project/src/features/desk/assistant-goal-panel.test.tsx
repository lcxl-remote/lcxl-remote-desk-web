import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AssistantGoalPanel, goalPlanningAction, goalProgressMessage } from './assistant-goal-panel';
import type { AiAssistantGoal } from './use-ai-assistant-chat';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));

const goal = { goalId: 'goal', state: 'running', goalText: '打开计算器并计算 6+8', checkpointSummary: '已输入 6+8' } as AiAssistantGoal;
const props = { connected: true, main: true, enabled: true, busy: false,
    onDetails: vi.fn(), onAction: vi.fn(), onReply: vi.fn(), onContinue: vi.fn() };

describe('AssistantGoalPanel', () => {
    it('starts with a single-line summary and status, without the full progress or actions', () => {
        render(<AssistantGoalPanel {...props} goal={{ ...goal, goalText: ' 验证目录\n\t并检查 文件 ' }} />);
        expect(screen.getByText('验证目录 并检查 文件')).toBeTruthy();
        expect(screen.getByText('Working')).toBeTruthy();
        expect(screen.getByRole('button', { name: 'Expand goal' }).getAttribute('aria-expanded')).toBe('false');
        expect(screen.queryByText('已输入 6+8')).toBeNull();
        expect(screen.queryByRole('button', { name: 'Details' })).toBeNull();
        expect(screen.queryByRole('button', { name: 'Pause goal' })).toBeNull();
        fireEvent.click(screen.getByRole('button', { name: 'Expand goal' }));
        expect(screen.getByRole('button', { name: 'Collapse goal' }).getAttribute('aria-expanded')).toBe('true');
        expect(screen.getByText('已输入 6+8')).toBeTruthy();
        fireEvent.click(screen.getByRole('button', { name: 'Collapse goal' }));
        expect(screen.queryByText('已输入 6+8')).toBeNull();
    });

    it('keeps goal and progress visible as the server state changes', () => {
        const { rerender } = render(<AssistantGoalPanel {...props} goal={goal} />);
        fireEvent.click(screen.getByRole('button', { name: 'Expand goal' }));
        expect(screen.getAllByText(goal.goalText)).toHaveLength(2);
        expect(screen.getByText('Working')).toBeTruthy();
        expect(screen.getByText('已输入 6+8')).toBeTruthy();
        rerender(<AssistantGoalPanel {...props} goal={{ ...goal, state: 'completed', checkpointSummary: '6+8=14' }} />);
        expect(screen.getByText('Completed')).toBeTruthy();
        expect(screen.getByText('6+8=14')).toBeTruthy();
        expect(screen.getByRole('button', { name: 'Collapse goal' })).toBeTruthy();
        expect(screen.queryByRole('button', { name: 'Cancel goal' })).toBeNull();
        fireEvent.click(screen.getByRole('button', { name: 'Goal is still incomplete' }));
        expect(props.onContinue).toHaveBeenCalled();
    });

    it.each(['completed', 'cancelled', 'failed'])('keeps a long %s goal collapsed on initial display', state => {
        render(<AssistantGoalPanel {...props} goal={{ ...goal, state, goalText: '一项很长的历史目标。'.repeat(100) }} />);
        expect(screen.getByRole('button', { name: 'Expand goal' }).getAttribute('aria-expanded')).toBe('false');
        expect(screen.queryByText('已输入 6+8')).toBeNull();
        expect(screen.queryByRole('button', { name: 'Cancel goal' })).toBeNull();
    });

    it('offers reply for a concrete question without resuming the goal', () => {
        render(<AssistantGoalPanel {...props} goal={{ ...goal, state: 'waiting_user', statusReason: '保存到哪里？' }} />);
        expect(screen.getByText('Waiting for user')).toBeTruthy();
        fireEvent.click(screen.getByRole('button', { name: 'Expand goal' }));
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
        fireEvent.click(screen.getByRole('button', { name: 'Expand goal' }));
        expect(screen.getAllByRole('button')).toHaveLength(2);
        fireEvent.click(screen.getByRole('button', { name: 'Details' }));
        expect(props.onDetails).toHaveBeenCalled();
    });

    it('keeps collapsed progress updates compact and permits offline inspection while disabling actions', () => {
        const { rerender } = render(<AssistantGoalPanel {...props} goal={goal} />);
        rerender(<AssistantGoalPanel {...props} enabled={false} connected={false} goal={{ ...goal, checkpointSummary: '新的进展' }} />);
        expect(screen.queryByText('新的进展')).toBeNull();
        fireEvent.click(screen.getByRole('button', { name: 'Expand goal' }));
        expect(screen.getByText('新的进展')).toBeTruthy();
        expect((screen.getByRole('button', { name: 'Pause goal' }) as HTMLButtonElement).disabled).toBe(true);
        expect((screen.getByRole('button', { name: 'Details' }) as HTMLButtonElement).disabled).toBe(false);
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
