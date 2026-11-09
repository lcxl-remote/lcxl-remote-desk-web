import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { afterEach, expect, it, vi } from 'vitest';
import { AiAssistantConversationChoices } from './ai-assistant-conversations';
import { PersonalAiAssistantPage } from './ai-assistant-page';
import { SIGNALING_TYPE_CODE_ASK_AI_ASSISTANT } from './constants';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(module => module.reactI18nextMock()));
const account = vi.hoisted(() => ({ user_id: null as number | null, name: 'Owner' }));
const signaling = vi.hoisted(() => ({ isConnected: true, subscribe: () => () => {},
    sendMessage: vi.fn().mockReturnValue('personal-request') }));
const connections = vi.hoisted(() => ({ data: [] as unknown[] }));
const selections = vi.hoisted(() => new Map<string, { revision: number; targetRefs: string[] }>());
vi.mock('@/services/clients', async importOriginal => ({
    ...await importOriginal<typeof import('@/services/clients')>(),
    getAiAssistantDeviceRoster: async ({ conversation }: { conversation: string }) => ({
        success: true, data: selections.get(conversation) ?? { revision: 0, targetRefs: [] },
    }),
    updateAiAssistantDeviceRoster: async ({ conversation, expectedRevision, targetRefs }: {
        conversation: string; expectedRevision: number; targetRefs: string[];
    }) => {
        if (expectedRevision !== (selections.get(conversation)?.revision ?? 0)) return { success: false };
        const data = { revision: expectedRevision + 1, targetRefs };
        selections.set(conversation, data);
        return { success: true, data };
    },
}));
vi.mock('./use-desk-signaling', () => ({ useDeskSignaling: () => signaling }));
vi.mock('@/services/hooks/userController/useGetCurrentUser', () => ({
    useGetCurrentUser: () => ({ data: { data: account } }),
}));
vi.mock('@/services/hooks/connectionController/useListConnections', () => ({
    useListConnections: () => ({ data: connections }),
}));
vi.mock('@/services/hooks/modelProviderController/useGetModelProvider', () => ({
    useGetModelProvider: () => ({ data: { data: { model: 'configured', api_key_set: true } } }),
}));
vi.mock('@/services/hooks/systemController/useQueryServerInfo', () => ({
    useQueryServerInfo: () => ({ data: { data: { ai_assistant: {
        turn_stream: true, capability_inventory: true, full_session_snapshot: true,
        permission_decision: true, approval_delegation: true, grant_revoke: true,
        background_task_cancel: true, object_context: true, exec_pty: true, device_settings: true,
    } } } }),
}));
afterEach(() => {
    vi.unstubAllGlobals(); localStorage.clear(); signaling.sendMessage.mockClear(); connections.data = []; selections.clear();
});

it('opens the assistant for the selected online device', () => {
    render(<MemoryRouter><AiAssistantConversationChoices
        devices={[{ connectionId: 'desk-1', deviceId: 'stable-device', name: 'Office Mac', description: 'macOS' }]}
        loading={false}
        error={false}
        refreshing={false}
        onRefresh={() => {}}
    /></MemoryRouter>);

    expect(screen.getByText('Office Mac')).toBeInTheDocument();
    expect(screen.getByRole('link', { name: /Open AI Assistant/ }))
        .toHaveAttribute('href', '/desk/device/stable-device/assistant');
});

it.each([null, 7])('opens and sends a personal conversation from an empty device list (account %s)', userId => {
    account.user_id = userId;
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({}) }));
    vi.stubGlobal('matchMedia', () => ({ matches: false, addEventListener() {}, removeEventListener() {} }));
    render(<QueryClientProvider client={new QueryClient()}><MemoryRouter initialEntries={['/ai-assistant/conversations']}>
        <Routes>
            <Route path="/ai-assistant/conversations" element={<AiAssistantConversationChoices personalEnabled
                devices={[]} loading={false} error={false} refreshing={false} onRefresh={() => {}} />} />
            <Route path="/ai-assistant/conversation" element={<PersonalAiAssistantPage />} />
        </Routes>
    </MemoryRouter></QueryClientProvider>);

    fireEvent.click(screen.getByRole('link', { name: 'Multi-device conversation' }));
    expect(signaling.sendMessage).not.toHaveBeenCalled();
    fireEvent.change(screen.getByRole('textbox'), { target: { value: 'Summarize this idea' } });
    fireEvent.click(screen.getByRole('button', { name: 'Send' }));
    expect(signaling.sendMessage).toHaveBeenCalledExactlyOnceWith(SIGNALING_TYPE_CODE_ASK_AI_ASSISTANT,
        expect.objectContaining({ question: 'Summarize this idea', start_goal: false }), undefined, expect.any(String));
    expect(localStorage.getItem(`ai-assistant-conversation:personal:${userId ?? 'owner'}`)).toBeTruthy();
});

it('restores the server selection in the same conversation on another client', async () => {
    connections.data = [
        { device_id: 'mac-ref', connection_id: 'mac-live', peer: { display_name: 'Office Mac', operation_system: 'Mac' }, ai_assistant_settings: { enabled: true } },
        { device_id: 'linux-ref', connection_id: 'linux-live', peer: { display_name: 'Linux test', operation_system: 'Linux' }, ai_assistant_settings: { enabled: true } },
        { device_id: 'disabled-ref', connection_id: 'disabled-live', peer: { display_name: 'Disabled PC', operation_system: 'Windows' }, ai_assistant_settings: { enabled: false } },
    ];
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({
        success: true, data: {
            sessionId: 'session', seq: 0, inputRevision: 0, controlRevision: 1, mainStopped: false, messages: [],
            subagents: { active_tasks: [], task: null, parent_session_id: null, attention_tasks: [], attention_count: 0 },
        },
    }) }));
    vi.stubGlobal('matchMedia', () => ({ matches: false, addEventListener() {}, removeEventListener() {} }));
    const openConversation = (id?: string) => render(<QueryClientProvider client={new QueryClient()}><MemoryRouter
        initialEntries={[`/ai-assistant/conversation${id ? `?conversation=${id}` : ''}`]}>
        <Routes><Route path="/ai-assistant/conversation" element={<PersonalAiAssistantPage />} /></Routes>
    </MemoryRouter></QueryClientProvider>);
    const initial = openConversation();

    fireEvent.keyDown(screen.getByRole('button', { name: 'Devices' }), { key: 'Enter' });
    const mac = screen.getByRole('menuitemcheckbox', { name: /Office Mac/ });
    const linux = screen.getByRole('menuitemcheckbox', { name: /Linux test/ });
    expect(screen.queryByRole('menuitemcheckbox', { name: /Disabled PC/ })).toBeNull();
    fireEvent.click(mac);
    await waitFor(() => expect(mac).toHaveAttribute('aria-checked', 'true'));
    fireEvent.click(linux);
    await waitFor(() => expect(linux).toHaveAttribute('aria-checked', 'true'));
    const [conversation] = selections.keys();
    initial.unmount();
    localStorage.clear();

    const reopened = openConversation(conversation);
    const restored = await screen.findByRole('button', { name: /^Devices\s*2$/ });
    await waitFor(() => expect(restored).toBeEnabled());
    fireEvent.keyDown(restored, { key: 'Enter' });
    expect(screen.getByRole('menuitemcheckbox', { name: /Office Mac/ })).toHaveAttribute('aria-checked', 'true');
    const restoredLinux = screen.getByRole('menuitemcheckbox', { name: /Linux test/ });
    expect(restoredLinux).toHaveAttribute('aria-checked', 'true');
    fireEvent.click(restoredLinux);
    await waitFor(() => expect(restoredLinux).toHaveAttribute('aria-checked', 'false'));
    reopened.unmount();

    const separate = openConversation('different-conversation');
    const empty = await screen.findByRole('button', { name: 'Devices' });
    await waitFor(() => expect(empty).toBeEnabled());
    fireEvent.keyDown(empty, { key: 'Enter' });
    expect(screen.getByRole('menuitemcheckbox', { name: /Office Mac/ })).toHaveAttribute('aria-checked', 'false');
    separate.unmount();

    openConversation(conversation);
    fireEvent.change(screen.getByRole('textbox'), { target: { value: 'Compare these devices' } });
    await waitFor(() => expect(screen.getByRole('button', { name: 'Send' })).toBeEnabled());
    fireEvent.click(screen.getByRole('button', { name: 'Send' }));
    // Capability lookups for the selected devices are separate requests; exactly one ask is sent.
    const asks = vi.mocked(signaling.sendMessage).mock.calls
        .filter(([type]) => type === SIGNALING_TYPE_CODE_ASK_AI_ASSISTANT);
    expect(asks).toEqual([[SIGNALING_TYPE_CODE_ASK_AI_ASSISTANT,
        expect.objectContaining({ question: 'Compare these devices', target_refs: ['mac-ref'] }),
        undefined, expect.any(String)]]);
});
