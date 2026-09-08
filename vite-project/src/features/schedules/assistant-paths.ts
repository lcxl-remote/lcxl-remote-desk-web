import type { ConnectionModel } from '@/services/types';

export function assistantConnections(connections: ConnectionModel[]): Record<string, string> {
    const paths: Record<string, string> = Object.create(null);
    const seen = new Set<string>();
    for (const connection of connections) {
        const device = connection.device_id ?? connection.version_info.client_id;
        if (!device || !connection.connection_id) continue;
        if (seen.has(device)) { delete paths[device]; continue; }
        seen.add(device);
        paths[device] = connection.connection_id;
    }
    return paths;
}

export function assistantPaths(connections: ConnectionModel[]): Record<string, string> {
    return Object.fromEntries(Object.entries(assistantConnections(connections))
        .map(([device, connection]) => [device, `/desk/${encodeURIComponent(connection)}/assistant`]));
}
