import { devicePath } from '@/features/desk/device-target-paths';
import type { ConnectionModel } from '@/services/types';

export function assistantConnections(connections: ConnectionModel[]): Record<string, string> {
    const paths: Record<string, string> = Object.create(null);
    const seen = new Set<string>();
    for (const connection of connections) {
        const device = connection.device_id;
        if (!device || !connection.connection_id) continue;
        if (seen.has(device)) { delete paths[device]; continue; }
        seen.add(device);
        paths[device] = connection.connection_id;
    }
    return paths;
}

export function assistantPaths(connections: ConnectionModel[]): Record<string, string> {
    return Object.fromEntries(Object.entries(assistantConnections(connections))
        .map(([device]) => [device, devicePath(device, 'assistant')]));
}

/** Online devices the owner can schedule, named as the host reports itself. */
export function scheduleDevices(connections: ConnectionModel[], online: ConnectionModel[]) {
    const paths = assistantPaths(online);
    const connectionIds = assistantConnections(online);
    return connections.flatMap(connection => {
        const id = connection.device_id;
        return id ? [{ id, name: connection.peer.display_name || id, connectionId: connectionIds[id], assistantPath: paths[id] }] : [];
    });
}
