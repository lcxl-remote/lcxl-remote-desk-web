import type { CapabilityInventoryEntry } from './use-device-assistant-capabilities';

export type ProviderInventoryState = 'ready' | 'degraded' | 'unavailable';

export type ProviderInventoryGroup = {
    providerId: string;
    displayNameKey: string;
    version: number;
    compiled: boolean;
    enabled: boolean;
    connected: boolean;
    readyCount: number;
    state: ProviderInventoryState;
    entries: CapabilityInventoryEntry[];
};

/**
 * Build a stable, secret-free Provider projection for the management surface.
 * Capability readiness remains authoritative; aggregation never promotes a
 * partially ready Provider to ready.
 */
export function groupCapabilityInventory(
    entries: CapabilityInventoryEntry[],
): ProviderInventoryGroup[] {
    const grouped = new Map<string, CapabilityInventoryEntry[]>();
    for (const entry of entries) {
        const current = grouped.get(entry.provider_id) ?? [];
        current.push(entry);
        grouped.set(entry.provider_id, current);
    }

    return [...grouped.entries()]
        .sort(([left], [right]) => left.localeCompare(right))
        .map(([providerId, providerEntries]) => {
            const sorted = [...providerEntries].sort((left, right) =>
                left.capability.capability_id.localeCompare(right.capability.capability_id),
            );
            const readyCount = sorted.filter((entry) => entry.ready).length;
            return {
                providerId,
                displayNameKey: sorted[0].provider_display_name_key,
                version: sorted[0].provider_version,
                compiled: sorted.every((entry) => entry.compiled),
                enabled: sorted.every((entry) => entry.enabled),
                connected: sorted.every((entry) => entry.connected),
                readyCount,
                state: readyCount === sorted.length
                    ? 'ready'
                    : readyCount > 0
                        ? 'degraded'
                        : 'unavailable',
                entries: sorted,
            };
        });
}
