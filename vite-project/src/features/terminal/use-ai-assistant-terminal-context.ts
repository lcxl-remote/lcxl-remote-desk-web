import { useCallback, useEffect, useRef, useState } from 'react';
import { v4 } from 'uuid';

import {
    SIGNALING_TYPE_CODE_AI_ASSISTANT_OBJECT_CONTEXT_UPDATED,
    SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_OBJECT_CONTEXT,
} from '@/features/desk/constants';
import type { SignalingSubscriber } from '@/features/desk/use-desk-signaling';

export type AssistantTerminalObjectRef = {
    token: string;
    snapshot_id: string;
    object_kind: 'terminal_output';
    expires_at: string;
};

type Props = {
    deskId: string;
    personalScope?: string;
    subscribe: (handler: SignalingSubscriber) => () => void;
    sendMessage: (
        type: number,
        data: unknown,
        connectionId?: string,
        requestId?: string,
    ) => string;
};

function storageKey(deskId: string) {
    return `ai-assistant-conversation:${deskId}`;
}

export function useAiAssistantTerminalContext({ deskId, personalScope, subscribe, sendMessage }: Props) {
    const [pending, setPending] = useState(false);
    const [added, setAdded] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const requestId = useRef<string | null>(null);
    const timer = useRef<number | null>(null);

    useEffect(() => subscribe((message) => {
        if (
            message.signaling_type !==
                SIGNALING_TYPE_CODE_AI_ASSISTANT_OBJECT_CONTEXT_UPDATED
            || message.request_id !== requestId.current
        ) return;
        if (timer.current !== null) window.clearTimeout(timer.current);
        timer.current = null;
        requestId.current = null;
        const ack = message.signaling_data as { error?: string | null };
        setPending(false);
        setAdded(!ack.error);
        setError(ack.error ?? null);
    }), [subscribe]);

    useEffect(() => () => {
        if (timer.current !== null) window.clearTimeout(timer.current);
    }, []);

    const attach = useCallback((
        objectRef: AssistantTerminalObjectRef,
        conversationTarget: 'fixed_device' | 'selectable_devices' = 'fixed_device',
    ) => {
        if (requestId.current) return false;
        const scope = conversationTarget === 'selectable_devices' ? personalScope : deskId;
        if (!scope) return false;
        let conversationId: string;
        try {
            const previous = localStorage.getItem(storageKey(scope));
            if (!previous && conversationTarget === 'selectable_devices') return false;
            conversationId = previous ?? v4();
            localStorage.setItem(storageKey(scope), conversationId);
        } catch {
            if (conversationTarget === 'selectable_devices') return false;
            conversationId = v4();
        }
        setPending(true);
        setAdded(false);
        setError(null);
        requestId.current = sendMessage(
            SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_OBJECT_CONTEXT,
            {
                conversation_id: conversationId,
                client_request_id: v4(),
                conversation_target: conversationTarget,
                operation: {
                    kind: 'attach_terminal_output',
                    object_ref: objectRef,
                    display_summary: 'Recent output from the current terminal',
                },
            },
            deskId,
        );
        timer.current = window.setTimeout(() => {
            requestId.current = null;
            timer.current = null;
            setPending(false);
            setError('timeout');
        }, 10_000);
        return true;
    }, [deskId, personalScope, sendMessage]);

    let personalAvailable = false;
    try {
        personalAvailable = !!personalScope && !!localStorage.getItem(storageKey(personalScope));
    } catch {
        // Access to local preferences may be disabled.
    }
    return { attach, pending, added, error, personalAvailable };
}
