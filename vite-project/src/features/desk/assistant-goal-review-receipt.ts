import type { AiAssistantMessage } from './use-ai-assistant-chat';

/** Only a server system event is eligible for this presentation. */
export function projectGoalReviewReceipt(message: { id: string; role: string; text: string }): AiAssistantMessage | null {
    if (message.role !== 'system_event') return null;
    try {
        const receipt = JSON.parse(message.text).goal_proposal_decision;
        if (!receipt || !['approved', 'denied', 'expired'].includes(receipt.status)
            || !['approval_ai', 'system'].includes(receipt.reviewer) || typeof receipt.reason !== 'string') return null;
        return { id: message.id, role: 'goal_decision', text: receipt.reason,
            goalDecision: { status: receipt.status, reviewer: receipt.reviewer } };
    } catch { return null; }
}
