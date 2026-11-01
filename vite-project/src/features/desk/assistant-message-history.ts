/** Older pages prepend records; the accepted newer representation wins overlaps. */
export function mergeAssistantMessages<T extends { id: string }>(earlier: T[], latest: T[]): T[] {
    const messages = new Map(earlier.map(message => [message.id, message]));
    for (const message of latest) messages.set(message.id, message);
    return [...messages.values()];
}

export type AssistantMessageFocus = {
    messageId: string;
    scroll?: boolean;
    parts?: { before: string; match: string; after: string } | null;
};

export function matchesAssistantMessage(message: { id: string; contextBoundaryIds?: string[] }, id?: string) {
    return id != null && (message.id === id || message.contextBoundaryIds?.includes(id) === true);
}
