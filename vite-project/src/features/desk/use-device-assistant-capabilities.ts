import { useCallback, useEffect, useRef, useState } from 'react';

import {
    SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_CAPABILITIES_UPDATED,
    SIGNALING_TYPE_CODE_GET_DEVICE_ASSISTANT_CAPABILITIES,
} from './constants';
import type { SignalingMessage, SignalingSubscriber } from './use-desk-signaling';

export type CapabilityBlockedReason =
    | 'disabled'
    | 'unsupported_platform'
    | 'version_mismatch'
    | 'edge_disconnected'
    | 'adapter_unavailable'
    | 'application_not_installed'
    | 'permission_missing'
    | 'office_bridge_not_paired'
    | 'no_active_document'
    | 'no_interactive_session'
    | 'local_ceiling'
    | 'busy';

export type CapabilityInventoryEntry = {
    provider_id: string;
    provider_display_name_key: string;
    provider_version: number;
    capability: {
        capability_id: string;
        tool_name: string;
        display_name_key: string;
        effect: string;
        execution_locality: string;
        execution_policy: unknown;
        limits: {
            max_input_bytes: number;
            max_output_bytes: number;
            max_objects: number;
            hard_timeout_ms: number;
        };
    };
    context_selectable: boolean;
    compiled: boolean;
    enabled: boolean;
    connected: boolean;
    ready: boolean;
    reason: CapabilityBlockedReason | null;
};

export type CapabilityInventorySnapshot = {
    schema_version: number;
    surface: 'oss_personal_owner' | 'manager_personal_owner';
    generated_at_unix_ms: number;
    entries: CapabilityInventoryEntry[];
};

type SendMessage = (
    type: number,
    data: unknown,
    toConnectionId?: string,
    requestId?: string,
) => string;

export function useDeviceAssistantCapabilities({
    deskId,
    subscribe,
    sendMessage,
    timeoutMs = 10_000,
}: {
    deskId: string | null;
    subscribe: (handler: SignalingSubscriber) => () => void;
    sendMessage: SendMessage;
    timeoutMs?: number;
}) {
    const [snapshot, setSnapshot] = useState<CapabilityInventorySnapshot | null>(null);
    const [loading, setLoading] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const requestId = useRef<string | null>(null);
    const timer = useRef<number | null>(null);

    useEffect(() => subscribe((message: SignalingMessage) => {
        if (
            message.signaling_type !== SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_CAPABILITIES_UPDATED
            || message.request_id !== requestId.current
        ) return;
        if (timer.current !== null) window.clearTimeout(timer.current);
        timer.current = null;
        requestId.current = null;
        setSnapshot(message.signaling_data as CapabilityInventorySnapshot);
        setLoading(false);
        setError(null);
    }), [subscribe]);

    useEffect(() => () => {
        if (timer.current !== null) window.clearTimeout(timer.current);
    }, []);

    const refresh = useCallback(() => {
        if (!deskId) return;
        if (timer.current !== null) window.clearTimeout(timer.current);
        setLoading(true);
        setError(null);
        requestId.current = sendMessage(
            SIGNALING_TYPE_CODE_GET_DEVICE_ASSISTANT_CAPABILITIES,
            {},
            deskId,
        );
        timer.current = window.setTimeout(() => {
            requestId.current = null;
            timer.current = null;
            setLoading(false);
            setError('timeout');
        }, timeoutMs);
    }, [deskId, sendMessage, timeoutMs]);

    return { snapshot, loading, error, refresh };
}
