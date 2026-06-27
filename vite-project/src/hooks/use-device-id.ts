import { useListConnections } from "@/services/hooks/connectionController/useListConnections"

/**
 * Resolves the device primary key for a connection from the live connection
 * list. Control ends address an enterprise manager device by this id so the
 * manager can route the request to the instance that owns the connection
 * (multi-instance reverse-proxy). The OSS single-instance signal server leaves
 * `device_id` unset, so this returns `undefined` there and callers fall back to
 * routing by `connection_id` (dual-target wire model).
 */
export function useDeviceId(connectionId: string | undefined): string | undefined {
    const { data: connections } = useListConnections()
    if (!connectionId) return undefined
    return connections?.find((c) => c.connection_id === connectionId)?.device_id ?? undefined
}
