export type AiAssistantFeatureProfile = {
    schema_version: number;
    turn_stream: boolean;
    capability_inventory: boolean;
    full_session_snapshot: boolean;
    permission_decision: boolean;
    approval_delegation: boolean;
    grant_revoke: boolean;
    background_task_cancel: boolean;
    object_context: boolean;
    exec_pty: boolean;
};

export const OSS_AI_ASSISTANT_FEATURES: AiAssistantFeatureProfile = {
    schema_version: 1,
    turn_stream: true,
    capability_inventory: true,
    full_session_snapshot: true,
    permission_decision: true,
    approval_delegation: true,
    grant_revoke: true,
    background_task_cancel: true,
    object_context: true,
    exec_pty: true,
};

export function hasAiAssistantBrowserEntry(
    profile: AiAssistantFeatureProfile | null | undefined,
): profile is AiAssistantFeatureProfile {
    return Boolean(
        profile
        && profile.turn_stream
        && profile.capability_inventory
        && profile.full_session_snapshot,
    );
}
