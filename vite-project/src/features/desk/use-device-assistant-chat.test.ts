import { act, renderHook, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
    SIGNALING_TYPE_CODE_ASK_DEVICE_ASSISTANT,
    SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_CONTEXT_UPDATED,
    SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_OBJECT_CONTEXT_UPDATED,
    SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_UPDATED,
    SIGNALING_TYPE_CODE_UPDATE_DEVICE_ASSISTANT_CONTEXT,
    SIGNALING_TYPE_CODE_UPDATE_DEVICE_ASSISTANT_OBJECT_CONTEXT,
} from './constants';
import type { SignalingSubscriber } from './use-desk-signaling';
import { useDeviceAssistantChat } from './use-device-assistant-chat';

describe('useDeviceAssistantChat', () => {
    beforeEach(() => localStorage.clear());
    afterEach(() => {
        vi.useRealTimers();
        vi.unstubAllGlobals();
    });

    it('restores browser-safe attachment metadata with the durable conversation', async () => {
        localStorage.setItem('device-assistant-conversation:desk-1', 'conversation-1');
        vi.stubGlobal('fetch', vi.fn(async () => ({
            ok: true,
            json: async () => ({
                data: {
                    messages: [],
                    contextAttachments: [{
                        id: 'attachment-1',
                        kind: 'interactive_session',
                        providerId: 'desktop.ui',
                        capabilityId: 'desktop.ui.inspect',
                        displaySummary: 'current interactive session',
                        createdAtUnixMs: 100,
                        expiresAtUnixMs: 200,
                        state: 'stale',
                        staleReason: 'worker_respawned',
                    }],
                },
            }),
        })));
        const { result } = renderHook(() => useDeviceAssistantChat({
            deskId: 'desk-1',
            subscribe: () => () => undefined,
            sendMessage: () => 'request',
        }));

        await waitFor(() => expect(result.current.hydrating).toBe(false));
        expect(result.current.attachments).toEqual([
            expect.objectContaining({
                capabilityId: 'desktop.ui.inspect',
                state: 'stale',
                staleReason: 'worker_respawned',
            }),
        ]);
    });

    it('discovers another tab conversation and polls its durable completion', async () => {
        vi.useFakeTimers();
        let snapshot = {
            active: true,
            messages: [{ id: 'user-1', role: 'user', text: 'shared request' }],
            contextAttachments: [],
            backgroundTasks: [{
                taskId: 'task-1',
                callId: 'call-1',
                providerId: 'office.document',
                capabilityId: 'office.document.inspect',
                toolName: 'inspect_excel_selection',
                effect: 'read_device',
                state: 'running',
                progressSequence: 2,
                supportsCancel: true,
                updatedAt: '2026-08-26T00:00:00Z',
            }],
        };
        vi.stubGlobal('fetch', vi.fn(async () => ({
            ok: true,
            json: async () => ({ data: snapshot }),
        })));
        const { result } = renderHook(() => useDeviceAssistantChat({
            deskId: 'desk-1',
            subscribe: () => () => undefined,
            sendMessage: () => 'request',
        }));

        await act(async () => {
            window.dispatchEvent(new StorageEvent('storage', {
                key: 'device-assistant-conversation:desk-1',
                newValue: 'shared-conversation',
            }));
            await Promise.resolve();
            await Promise.resolve();
        });
        expect(result.current.running).toBe(true);
        expect(result.current.messages.at(-1)?.text).toBe('shared request');
        expect(result.current.backgroundTasks).toEqual([
            expect.objectContaining({ taskId: 'task-1', state: 'running', progressSequence: 2 }),
        ]);

        snapshot = {
            active: false,
            messages: [
                { id: 'user-1', role: 'user', text: 'shared request' },
                { id: 'assistant-1', role: 'assistant', text: 'shared answer' },
            ],
            contextAttachments: [],
            backgroundTasks: [{
                taskId: 'task-1',
                callId: 'call-1',
                providerId: 'office.document',
                capabilityId: 'office.document.inspect',
                toolName: 'inspect_excel_selection',
                effect: 'read_device',
                state: 'succeeded',
                progressSequence: 3,
                supportsCancel: true,
                updatedAt: '2026-08-26T00:00:02Z',
                terminalAt: '2026-08-26T00:00:02Z',
            }],
        };
        await act(async () => {
            await vi.advanceTimersByTimeAsync(2_000);
        });
        expect(result.current.running).toBe(false);
        expect(result.current.status).toBe('done');
        expect(result.current.messages.at(-1)?.text).toBe('shared answer');
        expect(result.current.backgroundTasks).toEqual([
            expect.objectContaining({ taskId: 'task-1', state: 'succeeded', progressSequence: 3 }),
        ]);
    });

    it('keeps two mounted pages on one durable snapshot and closing one does not cancel the run', async () => {
        vi.useFakeTimers();
        localStorage.setItem('device-assistant-conversation:device-1', 'shared-conversation');
        let snapshot = {
            active: true,
            latestInputSeq: 3,
            handledInputSeq: 2,
            messages: [{ id: 'user-1', role: 'user', text: 'shared request' }],
            contextAttachments: [],
            permissionRequests: [],
            backgroundTasks: [{
                taskId: 'task-1',
                callId: 'call-1',
                providerId: 'browser.control',
                capabilityId: 'browser.control.semantic',
                toolName: 'prepare_gmail_web_draft_handoff',
                effect: 'write_external_draft',
                state: 'running',
                progressSequence: 4,
                supportsCancel: true,
                updatedAt: '2026-08-29T00:00:00Z',
            }],
            capabilityGrants: [{
                grantId: 'grant-1',
                providerId: 'browser.control',
                capabilityId: 'browser.control.semantic',
                toolName: 'prepare_gmail_web_draft_handoff',
                riskTier: 'r2',
                resourceScope: ['browser-extension-surface'],
                operationScope: ['write_external_draft'],
                remainingUses: 1,
                expiresAtUnixMs: 999,
                revokedAtUnixMs: null,
            }],
            unresolvedOutcome: null,
        };
        vi.stubGlobal('fetch', vi.fn(async () => ({
            ok: true,
            json: async () => ({ data: snapshot }),
        })));
        const firstSend = vi.fn(() => 'first-request');
        const secondSend = vi.fn(() => 'second-request');
        const first = renderHook(() => useDeviceAssistantChat({
            deskId: 'connection-1',
            conversationStorageScope: 'device-1',
            subscribe: () => () => undefined,
            sendMessage: firstSend,
        }));
        const second = renderHook(() => useDeviceAssistantChat({
            deskId: 'connection-1',
            conversationStorageScope: 'device-1',
            subscribe: () => () => undefined,
            sendMessage: secondSend,
        }));
        await act(async () => {
            await Promise.resolve();
            await Promise.resolve();
        });

        expect(first.result.current.messages).toEqual(second.result.current.messages);
        expect(first.result.current.backgroundTasks).toEqual(second.result.current.backgroundTasks);
        expect(first.result.current.capabilityGrants).toEqual(second.result.current.capabilityGrants);
        expect(first.result.current.pendingInputCount).toBe(1);
        expect(second.result.current.pendingInputCount).toBe(1);

        first.unmount();
        snapshot = {
            ...snapshot,
            active: false,
            handledInputSeq: 3,
            messages: [
                snapshot.messages[0],
                { id: 'assistant-1', role: 'assistant', text: 'shared answer' },
            ],
            backgroundTasks: [{
                ...snapshot.backgroundTasks[0],
                state: 'succeeded',
                progressSequence: 5,
                terminalAt: '2026-08-29T00:00:02Z',
            }],
        };
        await act(async () => {
            await vi.advanceTimersByTimeAsync(2_000);
        });

        expect(firstSend).not.toHaveBeenCalled();
        expect(secondSend).not.toHaveBeenCalled();
        expect(second.result.current.running).toBe(false);
        expect(second.result.current.status).toBe('done');
        expect(second.result.current.messages.at(-1)?.text).toBe('shared answer');
        expect(second.result.current.pendingInputCount).toBe(0);
        expect(second.result.current.backgroundTasks[0]).toEqual(
            expect.objectContaining({ state: 'succeeded', progressSequence: 5 }),
        );
    });

    it('persists context independently and blocks a turn until the ack arrives', async () => {
        let subscriber: SignalingSubscriber | null = null;
        vi.stubGlobal('fetch', vi.fn(async () => ({
            ok: true,
            json: async () => ({
                data: {
                    active: false,
                    messages: [],
                    contextAttachments: [{
                        id: 'attachment-1',
                        kind: 'interactive_session',
                        providerId: 'desktop.session',
                        capabilityId: 'desktop.session.inspect',
                        displaySummary: 'current interactive session',
                        createdAtUnixMs: 100,
                        expiresAtUnixMs: 200,
                        state: 'active',
                    }],
                },
            }),
        })));
        const sendMessage = vi.fn((type: number) => type ===
            SIGNALING_TYPE_CODE_UPDATE_DEVICE_ASSISTANT_CONTEXT
            ? 'context-request-1'
            : 'assistant-request-1');
        const { result } = renderHook(() => useDeviceAssistantChat({
            deskId: 'desk-1',
            subscribe: (handler) => {
                subscriber = handler;
                return () => { subscriber = null; };
            },
            sendMessage,
        }));

        act(() => {
            expect(result.current.updateContext(['desktop.session.inspect'])).toBe(true);
        });
        expect(result.current.contextUpdating).toBe(true);
        expect(result.current.start('must wait')).toBe(false);
        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_UPDATE_DEVICE_ASSISTANT_CONTEXT,
            expect.objectContaining({
                client_request_id: expect.any(String),
                selected_capability_ids: ['desktop.session.inspect'],
            }),
            'desk-1',
        );

        act(() => {
            subscriber?.({
                request_id: 'context-request-1',
                signaling_type: SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_CONTEXT_UPDATED,
                signaling_data: { changed: true },
            });
        });
        await waitFor(() => expect(result.current.contextUpdating).toBe(false));
        await waitFor(() => expect(result.current.attachments).toEqual([
            expect.objectContaining({ capabilityId: 'desktop.session.inspect', state: 'active' }),
        ]));
        act(() => {
            expect(result.current.start('now continue')).toBe(true);
        });
    });

    it('detaches object context explicitly and sends only persisted attachment ids', async () => {
        localStorage.setItem('device-assistant-conversation:desk-1', 'conversation-1');
        let subscriber: SignalingSubscriber | null = null;
        vi.stubGlobal('fetch', vi.fn(async () => ({
            ok: true,
            json: async () => ({
                data: {
                    active: false,
                    messages: [],
                    contextAttachments: [{
                        id: 'file-attachment-1',
                        kind: 'file',
                        providerId: 'file.workspace',
                        capabilityId: 'file.metadata.read',
                        displaySummary: 'selected.txt',
                        createdAtUnixMs: 100,
                        expiresAtUnixMs: 200,
                        state: 'active',
                    }],
                },
            }),
        })));
        const sendMessage = vi.fn((type: number) => type ===
            SIGNALING_TYPE_CODE_UPDATE_DEVICE_ASSISTANT_OBJECT_CONTEXT
            ? 'object-context-request-1'
            : 'assistant-request-1');
        const { result } = renderHook(() => useDeviceAssistantChat({
            deskId: 'desk-1',
            subscribe: (handler) => {
                subscriber = handler;
                return () => { subscriber = null; };
            },
            sendMessage,
        }));

        await waitFor(() => expect(result.current.attachments).toHaveLength(1));
        act(() => {
            expect(result.current.detachAttachment('file-attachment-1')).toBe(true);
        });
        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_UPDATE_DEVICE_ASSISTANT_OBJECT_CONTEXT,
            expect.objectContaining({
                conversation_id: 'conversation-1',
                operation: { kind: 'detach', attachment_id: 'file-attachment-1' },
            }),
            'desk-1',
        );
        act(() => {
            subscriber?.({
                request_id: 'object-context-request-1',
                signaling_type: SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_OBJECT_CONTEXT_UPDATED,
                signaling_data: { changed: true },
            });
        });
        await waitFor(() => expect(result.current.contextUpdating).toBe(false));

        act(() => {
            expect(result.current.start('Read the selected metadata.')).toBe(true);
        });
        expect(sendMessage).toHaveBeenLastCalledWith(
            SIGNALING_TYPE_CODE_ASK_DEVICE_ASSISTANT,
            expect.objectContaining({ selected_attachment_ids: ['file-attachment-1'] }),
            'desk-1',
        );
    });

    it('streams a read-only answer and exposes a validated typed preview event', () => {
        let subscriber: SignalingSubscriber | null = null;
        const subscribe = (handler: SignalingSubscriber) => {
            subscriber = handler;
            return () => { subscriber = null; };
        };
        const sendMessage = vi.fn(() => 'assistant-request-1');
        const { result } = renderHook(() => useDeviceAssistantChat({
            deskId: 'desk-1',
            subscribe,
            sendMessage,
        }));

        act(() => {
            expect(result.current.start(
                'Inspect Word and prepare a focus preview.',
                'en-US',
                ['desktop.ui.inspect'],
            )).toBe(true);
        });
        expect(sendMessage).toHaveBeenCalledWith(
            SIGNALING_TYPE_CODE_ASK_DEVICE_ASSISTANT,
            expect.objectContaining({
                question: 'Inspect Word and prepare a focus preview.',
                locale: 'en-US',
                selected_capability_ids: ['desktop.ui.inspect'],
            }),
            'desk-1',
        );

        const draft = {
            schema_version: 1,
            adapter: { kind: 'windows_uia', version: 'a4-windows-uia-read/v1' },
            risk: 'low',
            reversible: true,
            data_egress: false,
            actions: [{
                target: {
                    token: 'opaque',
                    snapshot_id: 'snapshot-1',
                    object_kind: 'ui_element',
                    expires_at: '2026-08-24T00:00:00Z',
                },
                action: { adapter: 'ui', action: { kind: 'focus' } },
                before_summary: 'Document surface is not focused.',
                after_intent: 'Focus the document surface.',
                verification: 'Inspect focus state again.',
            }],
        };
        act(() => {
            subscriber?.({
                request_id: 'assistant-request-1',
                signaling_type: SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_UPDATED,
                signaling_data: {
                    request_id: 'assistant-request-1',
                    seq: 0,
                    kind: 'status',
                    status: 'modeling',
                },
            });
            subscriber?.({
                request_id: 'assistant-request-1',
                signaling_type: SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_UPDATED,
                signaling_data: {
                    request_id: 'assistant-request-1',
                    seq: 1,
                    kind: 'tool_started',
                    tool_name: 'preview_computer_action',
                    tool_call_id: 'draft-1',
                    tool_arguments_json: JSON.stringify(draft),
                },
            });
            subscriber?.({
                request_id: 'assistant-request-1',
                signaling_type: SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_UPDATED,
                signaling_data: {
                    request_id: 'assistant-request-1',
                    seq: 2,
                    kind: 'tool_finished',
                    tool_call_id: 'draft-1',
                    tool_ok: true,
                    tool_output: JSON.stringify(draft),
                },
            });
            subscriber?.({
                request_id: 'assistant-request-1',
                signaling_type: SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_UPDATED,
                signaling_data: {
                    request_id: 'assistant-request-1',
                    seq: 3,
                    kind: 'answer',
                    answer: 'I prepared a focus preview. Nothing was executed.',
                },
            });
        });

        expect(result.current.draft?.actions).toHaveLength(1);
        expect(result.current.messages.at(-1)?.text).toContain('Nothing was executed');
        expect(result.current.running).toBe(false);
        expect(result.current.status).toBe('done');
    });

    it('ignores a stale assistant stream', () => {
        let subscriber: SignalingSubscriber | null = null;
        const { result } = renderHook(() => useDeviceAssistantChat({
            deskId: 'desk-1',
            subscribe: (handler) => {
                subscriber = handler;
                return () => { subscriber = null; };
            },
            sendMessage: () => 'current',
        }));
        act(() => { result.current.start('Inspect the current UI.'); });
        act(() => {
            subscriber?.({
                request_id: 'stale',
                signaling_type: SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_UPDATED,
                signaling_data: {
                    request_id: 'stale',
                    seq: 99,
                    kind: 'answer',
                    answer: 'stale answer',
                },
            });
        });
        expect(result.current.messages.some((message) => message.text === 'stale answer')).toBe(false);
        expect(result.current.running).toBe(true);
    });

    it('accepts a follow-up while running and observes only the newest request stream', () => {
        let subscriber: SignalingSubscriber | null = null;
        const sendMessage = vi
            .fn()
            .mockReturnValueOnce('request-1')
            .mockReturnValueOnce('request-2');
        const { result } = renderHook(() => useDeviceAssistantChat({
            deskId: 'desk-1',
            subscribe: (handler) => {
                subscriber = handler;
                return () => { subscriber = null; };
            },
            sendMessage,
        }));

        act(() => {
            expect(result.current.start('first request')).toBe(true);
            expect(result.current.start('new requirement')).toBe(true);
        });
        expect(result.current.messages.map((message) => message.text)).toEqual([
            'first request',
            'new requirement',
        ]);
        act(() => {
            subscriber?.({
                request_id: 'request-1',
                signaling_type: SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_UPDATED,
                signaling_data: {
                    request_id: 'request-1',
                    seq: 9,
                    kind: 'answer',
                    answer: 'stale answer',
                },
            });
            subscriber?.({
                request_id: 'request-2',
                signaling_type: SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_UPDATED,
                signaling_data: {
                    request_id: 'request-2',
                    seq: 1,
                    kind: 'answer',
                    answer: 'new answer',
                },
            });
        });
        expect(result.current.messages.map((message) => message.text)).toEqual([
            'first request',
            'new requirement',
            'new answer',
        ]);
        expect(result.current.running).toBe(false);
    });

    it('hydrates a durable permission request and posts one complete mixed decision', async () => {
        localStorage.setItem('device-assistant-conversation:desk-1', 'conversation-1');
        let state = 'pending';
        const request = {
            schemaVersion: 1,
            requestId: 'permission-1',
            inputRevision: 3,
            state,
            createdAt: '2026-08-26T00:00:00Z',
            items: [{
                itemId: 'inspect',
                providerId: 'desktop.session',
                toolName: 'inspect_desktop_session',
                expectedEffect: 'read_device',
                resourceScope: ['target:device-1'],
                operationScope: ['observe'],
                exportDestinations: [],
                suggestedTtlSeconds: 300,
                suggestedMaxUses: 1,
                reason: 'Inspect the current target',
            }, {
                itemId: 'inspect-ui',
                providerId: 'desktop.ui',
                toolName: 'inspect_desktop_ui',
                expectedEffect: 'read_device',
                resourceScope: ['target:device-1'],
                operationScope: ['observe-ui'],
                exportDestinations: [],
                suggestedTtlSeconds: 120,
                suggestedMaxUses: 1,
                reason: 'Inspect the current UI tree',
            }],
        };
        const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
            if (String(input).endsWith('/permission-decision')) {
                const body = JSON.parse(String(init?.body));
                expect(body).toEqual({
                    connection: 'desk-1',
                    conversation: 'conversation-1',
                    requestId: 'permission-1',
                    items: [{
                        itemId: 'inspect',
                        decision: 'approve',
                        resource_scope: [],
                        operation_scope: ['observe'],
                        export_destinations: [],
                        ttl_seconds: 60,
                        max_uses: 1,
                    }, {
                        itemId: 'inspect-ui',
                        decision: 'deny',
                    }],
                });
                state = 'partially_approved';
                return { ok: true, json: async () => ({ success: true, data: { state } }) };
            }
            return {
                ok: true,
                json: async () => ({
                    data: {
                        active: false,
                        inputRevision: 3,
                        messages: [{ id: 'user-1', role: 'user', text: 'inspect it' }],
                        permissionRequests: [{ ...request, state }],
                        contextAttachments: [],
                    },
                }),
            };
        });
        vi.stubGlobal('fetch', fetchMock);
        const { result } = renderHook(() => useDeviceAssistantChat({
            deskId: 'desk-1',
            subscribe: () => () => undefined,
            sendMessage: () => 'request',
        }));

        await waitFor(() => expect(result.current.permissionRequests[0]?.state).toBe('pending'));
        expect(result.current.status).toBe('permission_required');
        await act(async () => {
            expect(await result.current.decidePermissionItems(
                result.current.permissionRequests[0],
                [{
                    itemId: 'inspect',
                    decision: 'approve',
                    resource_scope: [],
                    operation_scope: ['observe'],
                    export_destinations: [],
                    ttl_seconds: 60,
                    max_uses: 1,
                }, {
                    itemId: 'inspect-ui',
                    decision: 'deny',
                }],
            )).toBe(true);
        });
        await waitFor(() => expect(result.current.permissionRequests[0]?.state).toBe('partially_approved'));
        expect(result.current.status).toBe('done');
        expect(result.current.error).toBeNull();
        expect(fetchMock).toHaveBeenCalledWith(
            '/api/my/device-assistant-session/permission-decision',
            expect.objectContaining({ method: 'POST' }),
        );
    });

    it('shows and revokes a durable capability grant without dispatching a tool', async () => {
        localStorage.setItem('device-assistant-conversation:desk-1', 'conversation-1');
        let revokedAtUnixMs: number | null = null;
        const grant = {
            grantId: 'grant-1',
            providerId: 'desktop.session',
            capabilityId: 'desktop.session.inspect',
            toolName: 'inspect_desktop_session',
            riskTier: 'r0',
            resourceScope: ['target:device-1'],
            operationScope: ['observe'],
            remainingUses: 1,
            expiresAtUnixMs: Date.now() + 60_000,
            revokedAtUnixMs,
            revokedReason: null,
        };
        const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
            if (String(input).endsWith('/capability-grant/revoke')) {
                expect(JSON.parse(String(init?.body))).toEqual({
                    connection: 'desk-1',
                    conversation: 'conversation-1',
                    grantId: 'grant-1',
                    reason: 'revoked_by_owner',
                });
                revokedAtUnixMs = Date.now();
                return {
                    ok: true,
                    json: async () => ({
                        success: true,
                        data: { ...grant, revokedAtUnixMs, revokedReason: 'revoked_by_owner' },
                    }),
                };
            }
            return {
                ok: true,
                json: async () => ({
                    data: {
                        active: false,
                        messages: [],
                        contextAttachments: [],
                        capabilityGrants: [{
                            ...grant,
                            revokedAtUnixMs,
                            revokedReason: revokedAtUnixMs ? 'revoked_by_owner' : null,
                        }],
                    },
                }),
            };
        });
        vi.stubGlobal('fetch', fetchMock);
        const { result } = renderHook(() => useDeviceAssistantChat({
            deskId: 'desk-1',
            subscribe: () => () => undefined,
            sendMessage: () => 'request',
        }));

        await waitFor(() => expect(result.current.capabilityGrants).toHaveLength(1));
        await act(async () => {
            expect(await result.current.revokeCapabilityGrant('grant-1')).toBe(true);
        });
        await waitFor(() => expect(
            result.current.capabilityGrants[0]?.revokedReason,
        ).toBe('revoked_by_owner'));
        expect(fetchMock).toHaveBeenCalledWith(
            '/api/my/device-assistant-session/capability-grant/revoke',
            expect.objectContaining({ method: 'POST' }),
        );
    });

    it('requires an explicit exact disposition before clearing an unknown outcome', async () => {
        localStorage.setItem('device-assistant-conversation:desk-1', 'conversation-1');
        let unresolved = true;
        const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
            if (String(input).endsWith('/outcome-unknown/dispose')) {
                expect(JSON.parse(String(init?.body))).toEqual({
                    connection: 'desk-1',
                    conversation: 'conversation-1',
                    workId: 94,
                    executionId: 'generation-94',
                });
                unresolved = false;
                return {
                    ok: true,
                    json: async () => ({ success: true, data: { disposed: true } }),
                };
            }
            return {
                ok: true,
                json: async () => ({
                    data: {
                        active: false,
                        messages: [{ id: 'user-1', role: 'user', text: 'continue' }],
                        contextAttachments: [],
                        unresolvedOutcome: unresolved ? {
                            workId: 94,
                            actionRequestId: 'action-94',
                            executionId: 'generation-94',
                            workKind: 'computer_action',
                        } : null,
                    },
                }),
            };
        });
        vi.stubGlobal('fetch', fetchMock);
        const { result } = renderHook(() => useDeviceAssistantChat({
            deskId: 'desk-1',
            subscribe: () => () => undefined,
            sendMessage: () => 'request',
        }));

        await waitFor(() => expect(result.current.status).toBe('outcome_unknown'));
        expect(result.current.unresolvedOutcome?.workId).toBe(94);
        await act(async () => {
            expect(await result.current.disposeUnknownOutcome()).toBe(true);
        });
        await waitFor(() => expect(result.current.unresolvedOutcome).toBeNull());
        expect(fetchMock).toHaveBeenCalledWith(
            '/api/my/device-assistant-session/outcome-unknown/dispose',
            expect.objectContaining({ method: 'POST' }),
        );
    });
});
