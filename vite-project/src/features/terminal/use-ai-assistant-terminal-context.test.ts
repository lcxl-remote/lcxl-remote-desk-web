import { act, renderHook, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

import {
    SIGNALING_TYPE_CODE_AI_ASSISTANT_OBJECT_CONTEXT_UPDATED,
    SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_OBJECT_CONTEXT,
} from '@/features/desk/constants';
import type { SignalingSubscriber } from '@/features/desk/use-desk-signaling';

import { useAiAssistantTerminalContext } from './use-ai-assistant-terminal-context';

describe('useAiAssistantTerminalContext', () => {
    beforeEach(() => localStorage.clear());

    it('attaches only the latest opaque edge reference and waits for its ack', async () => {
        localStorage.setItem('ai-assistant-conversation:desk-1', 'conversation-1');
        let subscriber: SignalingSubscriber | null = null;
        const sendMessage = vi.fn(() => 'terminal-context-request-1');
        const { result } = renderHook(() => useAiAssistantTerminalContext({
            deskId: 'desk-1',
            subscribe: (handler) => {
                subscriber = handler;
                return () => { subscriber = null; };
            },
            sendMessage,
        }));

        act(() => {
            expect(result.current.attach({
                token: 'edge-terminal-token',
                snapshot_id: 'worker-1:7',
                object_kind: 'terminal_output',
                expires_at: '2026-08-25T20:00:00Z',
            })).toBe(true);
        });

        expect(result.current.pending).toBe(true);
        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_OBJECT_CONTEXT,
            {
                conversation_id: 'conversation-1',
                client_request_id: expect.any(String),
                conversation_target: 'fixed_device',
                operation: {
                    kind: 'attach_terminal_output',
                    object_ref: {
                        token: 'edge-terminal-token',
                        snapshot_id: 'worker-1:7',
                        object_kind: 'terminal_output',
                        expires_at: '2026-08-25T20:00:00Z',
                    },
                    display_summary: 'Recent output from the current terminal',
                },
            },
            'desk-1',
        );
        expect(sendMessage.mock.calls[0][1]).not.toHaveProperty('content');
        expect(sendMessage.mock.calls[0][1]).not.toHaveProperty('terminal_id');

        act(() => {
            subscriber?.({
                request_id: 'terminal-context-request-1',
                signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_OBJECT_CONTEXT_UPDATED,
                signaling_data: { changed: true, error: null },
            });
        });
        await waitFor(() => expect(result.current.pending).toBe(false));
        expect(result.current.added).toBe(true);
        expect(result.current.error).toBeNull();
    });

    it('sends the terminal reference from this device to the current multi-device chat', () => {
        localStorage.setItem('ai-assistant-conversation:desk-1', 'fixed-chat');
        localStorage.setItem('ai-assistant-conversation:personal:7', 'shared-chat');
        const sendMessage = vi.fn(() => 'shared-context-request');
        const { result } = renderHook(() => useAiAssistantTerminalContext({
            deskId: 'desk-1', personalScope: 'personal:7',
            subscribe: () => () => {}, sendMessage,
        }));
        const reference = {
            token: 'opaque', snapshot_id: 'current', object_kind: 'terminal_output' as const,
            expires_at: '2030-01-01T00:00:00Z',
        };
        expect(result.current.personalAvailable).toBe(true);
        act(() => expect(result.current.attach(reference, 'selectable_devices')).toBe(true));
        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_OBJECT_CONTEXT,
            expect.objectContaining({
                conversation_id: 'shared-chat',
                conversation_target: 'selectable_devices',
                operation: expect.objectContaining({ object_ref: reference }),
            }),
            'desk-1',
        );
    });
});
