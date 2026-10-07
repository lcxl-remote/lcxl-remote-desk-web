import type { AiAssistantSubAgentSummary } from '@/services/types';
import type { PersistedSnapshot } from './use-ai-assistant-chat';

type Message = PersistedSnapshot['messages'][number];
export type SubagentHistory = {
    messages: Message[];
    nextBefore: string | null;
    hasMore: boolean;
};

export function sameSubagentHistoryScope(a: AiAssistantSubAgentSummary, b: AiAssistantSubAgentSummary) {
    return a.task_id === b.task_id && a.child_session_id === b.child_session_id && a.group_id === b.group_id
        && a.input_revision === b.input_revision && a.control_revision === b.control_revision
        && a.state_revision === b.state_revision && a.state === b.state;
}

function mergeMessages(earlier: Message[], latest: Message[]) {
    const byId = new Map(earlier.map(message => [message.id, message]));
    for (const message of latest) byId.set(message.id, message);
    return [...byId.values()];
}

/** Latest polling updates content without resetting an older or exhausted cursor. */
export function refreshSubagentHistory(prior: SubagentHistory | null, snapshot: PersistedSnapshot): SubagentHistory {
    return {
        messages: mergeMessages(prior?.messages ?? [], snapshot.messages),
        nextBefore: prior ? prior.nextBefore : snapshot.messagePage?.nextBeforeMessageId ?? null,
        hasMore: prior ? prior.hasMore : snapshot.messagePage?.hasMore ?? false,
    };
}

/** An older history page supplies presentation only, never fresh permissions or native authority. */
export function prependSubagentHistory(prior: SubagentHistory, page: PersistedSnapshot): SubagentHistory {
    return {
        messages: mergeMessages(page.messages, prior.messages),
        nextBefore: page.messagePage?.nextBeforeMessageId ?? null,
        hasMore: page.messagePage?.hasMore ?? false,
    };
}
