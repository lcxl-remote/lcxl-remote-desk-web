import { useCallback, useEffect, useRef, useState } from 'react';
import { v4 } from 'uuid';

import {
    SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_OBJECT_CONTEXT_UPDATED,
    SIGNALING_TYPE_CODE_UPDATE_DEVICE_ASSISTANT_OBJECT_CONTEXT,
} from '@/features/desk/constants';
import { useDeskSignaling } from '@/features/desk/use-desk-signaling';

export type AssistantFileObjectRef = {
    token: string;
    snapshot_id: string;
    object_kind: 'file' | 'directory';
    expires_at: string;
};

function storageKey(deskId: string) {
    return `device-assistant-conversation:${deskId}`;
}

export function useDeviceAssistantFileContext(
    deskId: string | undefined,
    conversationStorageScope = deskId,
) {
    const { isConnected, subscribe, sendMessage } = useDeskSignaling();
    const [pendingPath, setPendingPath] = useState<string | null>(null);
    const [lastAddedPath, setLastAddedPath] = useState<string | null>(null);
    const [error, setError] = useState<string | null>(null);
    const requestId = useRef<string | null>(null);
    const pendingPathRef = useRef<string | null>(null);
    const timer = useRef<number | null>(null);

    useEffect(() => subscribe((message) => {
        if (
            message.signaling_type !==
                SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_OBJECT_CONTEXT_UPDATED
            || message.request_id !== requestId.current
        ) return;
        if (timer.current !== null) window.clearTimeout(timer.current);
        timer.current = null;
        requestId.current = null;
        const ack = message.signaling_data as { error?: string | null };
        if (!ack.error) setLastAddedPath(pendingPathRef.current);
        pendingPathRef.current = null;
        setPendingPath(null);
        setError(ack.error ?? null);
    }), [subscribe]);

    useEffect(() => () => {
        if (timer.current !== null) window.clearTimeout(timer.current);
    }, []);

    const attach = useCallback((
        path: string,
        displaySummary: string,
        objectRef: AssistantFileObjectRef,
    ) => {
        if (!deskId || !isConnected || requestId.current) return false;
        let conversationId: string | null = null;
        try {
            conversationId = localStorage.getItem(storageKey(conversationStorageScope ?? deskId));
            if (!conversationId) {
                conversationId = v4();
                localStorage.setItem(storageKey(conversationStorageScope ?? deskId), conversationId);
            }
        } catch {
            conversationId = v4();
        }
        setPendingPath(path);
        pendingPathRef.current = path;
        setLastAddedPath(null);
        setError(null);
        requestId.current = sendMessage(
            SIGNALING_TYPE_CODE_UPDATE_DEVICE_ASSISTANT_OBJECT_CONTEXT,
            {
                conversation_id: conversationId,
                client_request_id: v4(),
                operation: {
                    kind: 'attach_file',
                    object_ref: objectRef,
                    display_summary: displaySummary,
                },
            },
            deskId,
        );
        timer.current = window.setTimeout(() => {
            requestId.current = null;
            timer.current = null;
            setPendingPath(null);
            pendingPathRef.current = null;
            setError('timeout');
        }, 10_000);
        return true;
    }, [conversationStorageScope, deskId, isConnected, sendMessage]);

    return { attach, isConnected, pendingPath, lastAddedPath, error };
}
