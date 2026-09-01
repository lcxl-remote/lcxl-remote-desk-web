import { describe, expect, it } from 'vitest';

import type { CapabilityInventoryEntry } from './use-device-assistant-capabilities';
import { groupCapabilityInventory } from './device-assistant-provider-inventory';

function entry(
    providerId: string,
    capabilityId: string,
    ready: boolean,
    reason: CapabilityInventoryEntry['reason'] = ready ? null : 'adapter_unavailable',
): CapabilityInventoryEntry {
    return {
        provider_id: providerId,
        provider_display_name_key: `assistant.provider.${providerId}`,
        provider_version: 1,
        capability: {
            capability_id: capabilityId,
            tool_name: `tool_${capabilityId}`,
            display_name_key: `assistant.capability.${capabilityId}`,
            effect: 'read_device',
            execution_locality: 'edge',
            execution_policy: 'inline_only',
            limits: {
                max_input_bytes: 1,
                max_output_bytes: 2,
                max_objects: 3,
                hard_timeout_ms: 4,
            },
        },
        context_selectable: false,
        compiled: true,
        enabled: true,
        connected: ready,
        ready,
        reason,
    };
}

describe('groupCapabilityInventory', () => {
    it('groups and sorts providers without promoting partial readiness', () => {
        const groups = groupCapabilityInventory([
            entry('provider.z', 'capability.z.ready', true),
            entry('provider.a', 'capability.a.unavailable', false),
            entry('provider.z', 'capability.z.blocked', false, 'permission_missing'),
        ]);

        expect(groups.map((group) => group.providerId)).toEqual(['provider.a', 'provider.z']);
        expect(groups[0]).toMatchObject({ state: 'unavailable', readyCount: 0 });
        expect(groups[1]).toMatchObject({
            state: 'degraded',
            readyCount: 1,
            compiled: true,
            enabled: true,
            connected: false,
        });
        expect(groups[1].entries.map((item) => item.capability.capability_id)).toEqual([
            'capability.z.blocked',
            'capability.z.ready',
        ]);
    });

    it('reports ready only when every capability is ready', () => {
        expect(groupCapabilityInventory([
            entry('provider.ready', 'capability.one', true),
            entry('provider.ready', 'capability.two', true),
        ])[0]).toMatchObject({ state: 'ready', readyCount: 2, connected: true });
    });
});
