import { expect, it } from 'vitest';
import { projectGoalReviewReceipt } from './assistant-goal-review-receipt';

it('projects a committed server decision and preserves its reason', () => {
    const event = { id: 'receipt', role: 'system_event', text: JSON.stringify({ goal_proposal_decision: {
        status: 'denied', reviewer: 'approval_ai', reason: 'The goal exceeds the requested scope', device_actions_authorized: false,
    } }) };
    expect(projectGoalReviewReceipt(event)).toMatchObject({ role: 'goal_decision', text: 'The goal exceeds the requested scope',
        goalDecision: { status: 'denied', reviewer: 'approval_ai' } });
    expect(projectGoalReviewReceipt({ ...event, role: 'assistant' })).toBeNull();
    expect(projectGoalReviewReceipt({ ...event, text: 'unrelated event' })).toBeNull();
});
