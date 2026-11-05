import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useState, type ComponentProps, type ReactElement } from 'react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { AssistantAutomaticApproval } from './assistant-automatic-approval';
import { useAiAssistantChat } from './use-ai-assistant-chat';
import { deskErrorCodeEnum } from '@/services/types';
import type { SignalingSubscriber } from './use-desk-signaling';
import { SIGNALING_TYPE_CODE_ASK_AI_ASSISTANT, SIGNALING_TYPE_CODE_AI_ASSISTANT_CONTEXT_UPDATED,
    SIGNALING_TYPE_CODE_AI_ASSISTANT_SESSION_SELECTED, SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_CONTEXT } from './constants';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
const approvalApi = vi.hoisted(() => ({ get: vi.fn(), update: vi.fn() }));
vi.mock('@/services/clients', async importOriginal => ({
    ...await importOriginal<typeof import('@/services/clients')>(),
    getAiAssistantApprovalMode: approvalApi.get,
    updateAiAssistantApprovalMode: approvalApi.update,
}));
type Chat = ComponentProps<typeof AssistantAutomaticApproval>['chat'];
const empty = (): Chat => ({ sessionId: undefined, hydrating: false, contextUpdating: false,
    approvalDelegation: null, approvalInitializing: false, approvalInitializationFailed: false,
    error: null, initializeApprovalConversation: vi.fn().mockResolvedValue(true) });
const renderWithMode = (ui: ReactElement) => render(<QueryClientProvider client={new QueryClient({
    defaultOptions: { queries: { retry: false } },
})}>{ui}</QueryClientProvider>);
const persisted = () => ({ sessionId: 'stored-session', seq: 1, inputRevision: 0, controlRevision: 1,
    mainStopped: false, active: false, messages: [],
    subagents: { active_tasks: [], attention_tasks: [], attention_count: 0, task: null, parent_session_id: null },
    approvalModelReadiness: { available: true }, approvalDelegation: null });
beforeEach(() => {
    localStorage.clear();
    vi.clearAllMocks();
    approvalApi.get.mockResolvedValue({ success: true, data: { mode: 'ai_review', revision: 1, aiReviewAvailable: true } });
});
afterEach(() => { vi.unstubAllGlobals(); vi.useRealTimers(); });

describe('automatic approval in a blank conversation', () => {
    it.each([
        { initialConversationId: undefined, storedConversationId: undefined },
        { initialConversationId: 'reserved-empty-conversation', storedConversationId: undefined },
        { initialConversationId: undefined, storedConversationId: 'stored-empty-conversation' },
    ])('initializes and shows the default approval with client selection %j and preserves it for the first message', async ({ initialConversationId, storedConversationId }) => {
        if (storedConversationId) localStorage.setItem('ai-assistant-conversation:stable-device', storedConversationId);
        let subscriber: SignalingSubscriber | undefined;
        const sendMessage = vi.fn((_type: number, _data: unknown, _to?: string, id?: string) => id ?? 'initialize-request');
        let chat: ReturnType<typeof useAiAssistantChat>;
        let initialized = false;
        const fetchMock = vi.fn(async () => {
            return { ok: true, json: async () => ({ success: true, data: initialized ? {
                ...persisted(), sessionId: 'empty-session',
            } : null, ...(initialized ? {} : { success: false, code: deskErrorCodeEnum.PERMISSION_ERROR }) }) };
        });
        vi.stubGlobal('fetch', fetchMock);
        function Workspace() {
            const [open, setOpen] = useState(false);
            chat = useAiAssistantChat({ deskId: 'device', connected: true, conversationStorageScope: 'stable-device', initialConversationId,
                subscribe: handler => { subscriber = handler; return () => {}; }, sendMessage });
            return <><button onClick={() => setOpen(true)}>Approval settings</button>
                {open && <AssistantAutomaticApproval chat={chat} enabled connected />}</>;
        }
        renderWithMode(<Workspace />);
        await waitFor(() => expect(chat!.hydrating).toBe(false));
        await act(async () => subscriber?.({ request_id: 'initialize-request',
            signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_SESSION_SELECTED,
            signaling_data: { target: null }, response_state: { error_code: deskErrorCodeEnum.SUCCESS } }));
        sendMessage.mockClear();
        fireEvent.click(screen.getByRole('button', { name: 'Approval settings' }));
        await waitFor(() => expect(sendMessage).toHaveBeenCalledOnce());
        expect(screen.queryByRole('combobox')).not.toBeInTheDocument();
        expect(screen.getByRole('status')).toBeInTheDocument();
        const payload = sendMessage.mock.calls[0][1] as { conversation_id: string; client_request_id: string };
        if (initialConversationId || storedConversationId) expect(payload.conversation_id).toBe(initialConversationId ?? storedConversationId);
        expect(sendMessage).toHaveBeenCalledWith(SIGNALING_TYPE_CODE_UPDATE_AI_ASSISTANT_CONTEXT,
            expect.objectContaining({ selected_capability_ids: [], conversation_id: expect.any(String) }), 'device');
        initialized = true;
        await act(async () => subscriber?.({ request_id: 'initialize-request',
            signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_CONTEXT_UPDATED,
            signaling_data: { changed: false, conversation_id: payload.conversation_id, client_request_id: payload.client_request_id } }));
        expect(await screen.findByRole('combobox', { name: 'Approval mode' })).toHaveTextContent('AI review');
        expect(approvalApi.get).toHaveBeenCalledWith({ session: 'empty-session' }, expect.anything());
        expect(approvalApi.update).not.toHaveBeenCalled();
        expect(chat!.messages).toEqual([]);
        expect(sendMessage.mock.calls.some(([type]) => type === SIGNALING_TYPE_CODE_ASK_AI_ASSISTANT)).toBe(false);
        act(() => { expect(chat!.start('My first real message')).toBe(true); });
        expect(sendMessage).toHaveBeenLastCalledWith(SIGNALING_TYPE_CODE_ASK_AI_ASSISTANT,
            expect.objectContaining({ conversation_id: payload.conversation_id, question: 'My first real message' }),
            'device', expect.any(String));
    });

    it.each(['network', 'malformed'])('shows a bounded %s failure and retries without clearing existing history', async failure => {
        let retry = false;
        let chat: ReturnType<typeof useAiAssistantChat>;
        const sendMessage = vi.fn().mockReturnValue('context-request');
        vi.stubGlobal('fetch', vi.fn(async () => ({
            ok: retry || failure !== 'network', json: async () => ({
                success: true, code: deskErrorCodeEnum.SUCCESS,
                data: retry ? { ...persisted(), messages: [{ id: 'answer', role: 'assistant', text: 'Existing answer' }] }
                    : { ...persisted(), controlRevision: -1 },
            }),
        })));
        function Workspace() {
            const [open, setOpen] = useState(false);
            chat = useAiAssistantChat({ deskId: 'device', initialConversationId: 'existing-client',
                subscribe: () => () => {}, sendMessage });
            return <><button onClick={() => setOpen(true)}>Approval settings</button>
                {open && <AssistantAutomaticApproval chat={chat} enabled connected />}</>;
        }
        renderWithMode(<Workspace />);
        await waitFor(() => expect(chat!.hydrating).toBe(false));
        fireEvent.click(screen.getByRole('button', { name: 'Approval settings' }));
        await waitFor(() => expect(screen.getByRole('alert')).toHaveTextContent(
            'Could not restore conversation'));
        expect(screen.queryByRole('status')).not.toBeInTheDocument();
        expect(sendMessage).not.toHaveBeenCalled();
        retry = true;
        fireEvent.click(screen.getByRole('button', { name: 'Retry', exact: true }));
        expect(await screen.findByRole('combobox', { name: 'Approval mode' })).toBeEnabled();
        expect(chat!.messages).toEqual([expect.objectContaining({ text: 'Existing answer' })]);
        expect(screen.queryByRole('alert')).not.toBeInTheDocument();
        expect(sendMessage).not.toHaveBeenCalled();
    });

    it('reports a missing snapshot after the initialization acknowledgement instead of loading forever or resending', async () => {
        let subscriber: SignalingSubscriber | undefined;
        const sendMessage = vi.fn().mockReturnValue('context-request');
        vi.stubGlobal('fetch', vi.fn(async () => ({ ok: true, json: async () => ({
            success: false, code: deskErrorCodeEnum.PERMISSION_ERROR, data: null,
        }) })));
        function Workspace() {
            const [open, setOpen] = useState(false);
            const chat = useAiAssistantChat({ deskId: 'device', subscribe: handler => { subscriber = handler; return () => {}; }, sendMessage });
            return <><button onClick={() => setOpen(true)}>Approval settings</button>
                {open && <AssistantAutomaticApproval chat={chat} enabled connected />}</>;
        }
        renderWithMode(<Workspace />);
        fireEvent.click(screen.getByRole('button', { name: 'Approval settings' }));
        await waitFor(() => expect(sendMessage).toHaveBeenCalledOnce());
        await act(async () => subscriber?.({ request_id: 'context-request',
            signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_CONTEXT_UPDATED, signaling_data: { changed: false } }));
        await waitFor(() => expect(screen.getByRole('alert')).toHaveTextContent('Could not load approval settings'));
        expect(screen.queryByRole('status')).not.toBeInTheDocument();
        expect(screen.getByRole('button', { name: 'Retry', exact: true })).toBeEnabled();
        expect(sendMessage).toHaveBeenCalledOnce();
    });

    it('ends loading and offers retry when the initialization acknowledgement never arrives', async () => {
        vi.useFakeTimers();
        const sendMessage = vi.fn().mockReturnValue('context-request');
        vi.stubGlobal('fetch', vi.fn(async () => ({ ok: true, json: async () => ({
            success: false, code: deskErrorCodeEnum.PERMISSION_ERROR, data: null,
        }) })));
        function Workspace() {
            const [open, setOpen] = useState(false);
            const chat = useAiAssistantChat({ deskId: 'device', subscribe: () => () => {}, sendMessage });
            return <><button onClick={() => setOpen(true)}>Approval settings</button>
                {open && <AssistantAutomaticApproval chat={chat} enabled connected />}</>;
        }
        renderWithMode(<Workspace />);
        await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Approval settings' })));
        expect(sendMessage).toHaveBeenCalledOnce();
        expect(screen.getByRole('status')).toBeInTheDocument();
        await act(async () => vi.advanceTimersByTimeAsync(10_000));
        expect(screen.getByRole('alert')).toHaveTextContent('Could not load approval settings');
        expect(screen.queryByRole('status')).not.toBeInTheDocument();
        expect(screen.getByRole('button', { name: 'Retry', exact: true })).toBeEnabled();
        expect(sendMessage).toHaveBeenCalledOnce();
    });

    it('keeps an existing conversation and its selected context unchanged', async () => {
        const chat = { ...empty(), sessionId: 'stored-existing' };
        renderWithMode(<AssistantAutomaticApproval chat={chat} enabled connected />);
        expect(await screen.findByRole('combobox', { name: 'Approval mode' })).toHaveTextContent('AI review');
        expect(chat.initializeApprovalConversation).not.toHaveBeenCalled();
    });

    it.each([{ enabled: false, connected: true }, { enabled: true, connected: false }])(
        'does not initialize while the assistant or signaling is unavailable: %j', props => {
            const chat = empty();
            renderWithMode(<AssistantAutomaticApproval chat={chat} {...props} />);
            expect(chat.initializeApprovalConversation).not.toHaveBeenCalled();
            expect(screen.queryByRole('combobox')).not.toBeInTheDocument();
        });

    it('keeps an existing approval mode accessible after signaling disconnects', async () => {
        const chat = { ...empty(), sessionId: 'stored-existing' };
        renderWithMode(<AssistantAutomaticApproval chat={chat} enabled connected={false} />);
        expect(await screen.findByRole('combobox', { name: 'Approval mode' })).toBeEnabled();
        expect(chat.initializeApprovalConversation).not.toHaveBeenCalled();
    });
});
