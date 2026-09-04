type DeviceAssistantProjection = {
    device_assistant_enabled?: boolean | null;
};

/** Missing projections fail closed because Device Assistant has no legacy compatibility mode. */
export function isDeviceAssistantEnabled(versionInfo?: DeviceAssistantProjection | null): boolean {
    return versionInfo?.device_assistant_enabled === true;
}
