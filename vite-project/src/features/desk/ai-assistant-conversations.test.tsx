import { fireEvent, render, screen } from '@testing-library/react';
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
afterEach(() => { vi.unstubAllGlobals(); localStorage.clear(); signaling.sendMessage.mockClear(); connections.data = []; });

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

it('submits checked devices as stable targets from the personal conversation', () => {
    connections.data = [
        { device_id: 'mac-ref', connection_id: 'mac-live', peer: { display_name: 'Office Mac', operation_system: 'Mac' }, ai_assistant_settings: { enabled: true } },
        { device_id: 'linux-ref', connection_id: 'linux-live', peer: { display_name: 'Linux test', operation_system: 'Linux' }, ai_assistant_settings: { enabled: true } },
        { device_id: 'disabled-ref', connection_id: 'disabled-live', peer: { display_name: 'Disabled PC', operation_system: 'Windows' }, ai_assistant_settings: { enabled: false } },
    ];
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({}) }));
    vi.stubGlobal('matchMedia', () => ({ matches: false, addEventListener() {}, removeEventListener() {} }));
    render(<QueryClientProvider client={new QueryClient()}><MemoryRouter initialEntries={['/ai-assistant/conversation']}>
        <Routes><Route path="/ai-assistant/conversation" element={<PersonalAiAssistantPage />} /></Routes>
    </MemoryRouter></QueryClientProvider>);

    fireEvent.keyDown(screen.getByRole('button', { name: 'Devices' }), { key: 'Enter' });
    const mac = screen.getByRole('menuitemcheckbox', { name: /Office Mac/ });
    const linux = screen.getByRole('menuitemcheckbox', { name: /Linux test/ });
    expect(screen.queryByRole('menuitemcheckbox', { name: /Disabled PC/ })).toBeNull();
    fireEvent.click(mac);
    fireEvent.click(linux);
    expect(mac).toHaveAttribute('aria-checked', 'true');
    expect(linux).toHaveAttribute('aria-checked', 'true');
    fireEvent.keyDown(mac, { key: 'Escape' });

    fireEvent.change(screen.getByRole('textbox'), { target: { value: 'Compare these devices' } });
    fireEvent.click(screen.getByRole('button', { name: 'Send' }));
    expect(signaling.sendMessage).toHaveBeenCalledExactlyOnceWith(SIGNALING_TYPE_CODE_ASK_AI_ASSISTANT,
        expect.objectContaining({ question: 'Compare these devices', target_refs: ['mac-ref', 'linux-ref'] }),
        undefined, expect.any(String));
});
