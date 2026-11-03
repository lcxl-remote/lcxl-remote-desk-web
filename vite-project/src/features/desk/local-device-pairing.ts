/** Pairing is available only for a freshly verified desktop hosted by this origin. */
export function canPairLocalDevice(deviceId: string | null | undefined, info: {
    isSuccess: boolean;
    isFetchedAfterMount: boolean;
    isFetching: boolean;
    isError: boolean;
    data?: { success: boolean; data?: { local_device_id?: string | null } | null };
}): boolean {
    return typeof deviceId === 'string' && deviceId.trim().length > 0
        && info.isSuccess && info.isFetchedAfterMount && !info.isFetching && !info.isError
        && info.data?.success === true && info.data.data?.local_device_id === deviceId;
}
