import { mergeAssistantMessages } from './assistant-message-history';
import type { AiAssistantSubAgentSummary } from '@/services/types';
import type { PersistedSnapshot } from './use-ai-assistant-chat';

type Message = PersistedSnapshot['messages'][number];
export type SubagentHistory = {
    messages: Message[];
    nextBefore: string | null;
    hasMore: boolean;
};

export function sameSubagentRequest(a: AiAssistantSubAgentSummary, b: AiAssistantSubAgentSummary) {
    return a.task_id === b.task_id && a.child_session_id === b.child_session_id && a.group_id === b.group_id
        && a.input_revision === b.input_revision && a.control_revision === b.control_revision;
}

export function sameSubagentHistoryScope(a: AiAssistantSubAgentSummary, b: AiAssistantSubAgentSummary) {
    return sameSubagentRequest(a, b) && a.state_revision === b.state_revision && a.state === b.state;
}

/** Latest polling updates content without resetting an older or exhausted cursor. */
export function refreshSubagentHistory(prior: SubagentHistory | null, snapshot: PersistedSnapshot): SubagentHistory {
    return {
        messages: mergeAssistantMessages(prior?.messages ?? [], snapshot.messages),
        nextBefore: prior ? prior.nextBefore : snapshot.messagePage?.nextBeforeMessageId ?? null,
        hasMore: prior ? prior.hasMore : snapshot.messagePage?.hasMore ?? false,
    };
}

/** An older history page supplies presentation only, never fresh permissions or native authority. */
export function prependSubagentHistory(prior: SubagentHistory, page: PersistedSnapshot): SubagentHistory {
    return {
        messages: mergeAssistantMessages(page.messages, prior.messages),
        nextBefore: page.messagePage?.nextBeforeMessageId ?? null,
        hasMore: page.messagePage?.hasMore ?? false,
    };
}
