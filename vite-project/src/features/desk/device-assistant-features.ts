export type DeviceAssistantFeatureProfile = {
    schema_version: number;
    turn_stream: boolean;
    capability_inventory: boolean;
    full_session_snapshot: boolean;
    permission_decision: boolean;
    grant_revoke: boolean;
    background_task_cancel: boolean;
    unknown_outcome_disposition: boolean;
    object_context: boolean;
    exec_pty: boolean;
};

export const OSS_DEVICE_ASSISTANT_FEATURES: DeviceAssistantFeatureProfile = {
    schema_version: 1,
    turn_stream: true,
    capability_inventory: true,
    full_session_snapshot: true,
    permission_decision: true,
    grant_revoke: true,
    background_task_cancel: true,
    unknown_outcome_disposition: true,
    object_context: true,
    exec_pty: true,
};

export function hasDeviceAssistantBrowserEntry(
    profile: DeviceAssistantFeatureProfile | null | undefined,
): profile is DeviceAssistantFeatureProfile {
    return Boolean(
        profile
        && profile.turn_stream
        && profile.capability_inventory
        && profile.full_session_snapshot,
    );
}
