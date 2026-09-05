/** Descriptions are presentation-only; never use this key as an executable capability. */
export function capabilityDescriptionKey(displayNameKey: string): string {
    return displayNameKey.startsWith('assistant.capability.')
        ? displayNameKey.replace('assistant.capability.', 'assistant.capabilityDescription.')
        : 'pages.deviceAssistant.workspace.descriptionUnavailable';
}
