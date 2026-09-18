type AiAssistantProjection = {
    ai_assistant_enabled?: boolean | null;
};

/** Missing projections fail closed because AI Assistant has no legacy compatibility mode. */
export function isAiAssistantEnabled(versionInfo?: AiAssistantProjection | null): boolean {
    return versionInfo?.ai_assistant_enabled === true;
}
