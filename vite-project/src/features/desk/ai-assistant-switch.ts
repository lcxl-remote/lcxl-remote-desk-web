type AiAssistantProjection = {
    enabled?: boolean | null;
};

/** Only an explicit device settings projection enables AI Assistant. */
export function isAiAssistantEnabled(settings?: AiAssistantProjection | null): boolean {
    return settings?.enabled === true;
}
