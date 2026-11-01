import { useListConnections } from "@/services/hooks/connectionController/useListConnections"
import type { ConnectionModel } from "@/services/types"

export type DeviceConnectionResolution =
    | { status: "loading"; connection: undefined }
    | { status: "persistent"; connection: ConnectionModel; deviceKey: string }
    | {
        status: "memory-only"
        connection: ConnectionModel | undefined
        deviceKey: null
    }

/** Resolve the live connection metadata for a remote desk. */
export function useDeviceConnection(connectionId: string | undefined) {
    const { data: connections } = useListConnections()
    if (!connectionId) return undefined
    return connections?.find((c) => c.connection_id === connectionId)
}

/**
 * Resolve the persistence identity without treating an initial undefined query
 * result as a permanently missing device. Callers may proceed in memory-only
 * mode only after the connections query has actually completed.
 */
export function useDeviceConnectionResolution(
    connectionId: string | undefined,
): DeviceConnectionResolution {
    const query = useListConnections()
    if (connectionId && query.isLoading) {
        return { status: "loading", connection: undefined }
    }
    const connection = connectionId
        ? query.data?.find((item) => item.connection_id === connectionId)
        : undefined
    const managerDeviceId = connection?.device_id?.trim()
    if (connection && managerDeviceId) {
        return {
            status: "persistent",
            connection,
            deviceKey: `device:${managerDeviceId}`,
        }
    }
    const standaloneClientId = connection?.version_info.client_id?.trim()
    if (connection && standaloneClientId) {
        return {
            status: "persistent",
            connection,
            deviceKey: `client:${standaloneClientId}`,
        }
    }
    return { status: "memory-only", connection, deviceKey: null }
}
