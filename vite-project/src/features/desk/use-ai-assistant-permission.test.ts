import { act, renderHook, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useAiAssistantChat } from './use-ai-assistant-chat';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
const request = { schemaVersion: 1, requestId: 'permission', inputRevision: 3, state: 'pending' as const,
    createdAt: '2026-09-15T00:00:00Z', items: [{ itemId: 'read', providerId: 'desktop.session',
        toolName: 'inspect_desktop_session', expectedEffect: 'read_device' as const, reason: 'Read selected device',
        resourceScope: ['target:device'], operationScope: ['observe'], exportDestinations: [],
        suggestedTtlSeconds: 60, suggestedMaxUses: 1 }] };
const subscribe = () => () => undefined;
const sendMessage = () => 'request';
const snapshot = (active = false) => ({ ok: true, json: async () => ({ data: {
    sessionId: 'session', seq: 1, inputRevision: 3, active, requestId: active ? 'running' : undefined,
    messages: [], contextAttachments: [], permissionRequests: [request],
} }) });
beforeEach(() => {
    localStorage.clear();
    localStorage.setItem('ai-assistant-conversation:desk-1', 'conversation-1');
    localStorage.setItem('ai-assistant-conversation:desk-2', 'conversation-2');
});
afterEach(() => vi.unstubAllGlobals());

describe('assistant permission submission fencing', () => {
    it.each(['committing', 'stale-input'])('does not approve %s requests', async state => {
        const fetch = vi.fn().mockResolvedValue(snapshot(state === 'committing')); vi.stubGlobal('fetch', fetch);
        const { result } = renderHook(() => useAiAssistantChat({ deskId: 'desk-1', connected: true, subscribe, sendMessage }));
        await waitFor(() => expect(result.current.permissionRequests).toHaveLength(1));
        await act(async () => expect(await result.current.decidePermission(
            state === 'stale-input' ? { ...request, inputRevision: 2 } : request, true,
        )).toBe(false));
        expect(fetch.mock.calls.some(([url]) => String(url).endsWith('/permission-decision'))).toBe(false);
    });

    it.each(['device', 'disconnect', 'reset'] as const)('ignores an old response after %s and blocks synchronous double submission', async change => {
        let finish!: (value: unknown) => void;
        const fetch = vi.fn((url: RequestInfo | URL) => String(url).endsWith('/permission-decision')
            ? new Promise(resolve => { finish = resolve; }) : Promise.resolve(snapshot()));
        vi.stubGlobal('fetch', fetch);
        const { result, rerender } = renderHook(({ deskId, connected }) => useAiAssistantChat({ deskId, connected, subscribe, sendMessage }),
            { initialProps: { deskId: 'desk-1', connected: true } });
        await waitFor(() => expect(result.current.permissionRequests).toHaveLength(1));
        let first!: Promise<boolean>;
        await act(async () => {
            first = result.current.decidePermission(request, true);
            expect(await result.current.decidePermission(request, true)).toBe(false);
        });
        expect(fetch.mock.calls.filter(([url]) => String(url).endsWith('/permission-decision'))).toHaveLength(1);
        if (change === 'reset') act(() => result.current.reset());
        else rerender({ deskId: change === 'device' ? 'desk-2' : 'desk-1', connected: change !== 'disconnect' });
        await act(async () => {});
        const reads = fetch.mock.calls.length;
        await act(async () => {
            finish({ ok: false, json: async () => ({ message: 'old request error' }) });
            expect(await first).toBe(false);
        });
        expect(fetch).toHaveBeenCalledTimes(reads);
        expect(result.current.error).toBeNull();
        expect(result.current.permissionUpdating).toBe(false);
    });

    it('reloads durable state after an uncertain response without repeating the decision', async () => {
        const fetch = vi.fn((url: RequestInfo | URL) => String(url).endsWith('/permission-decision')
            ? Promise.reject(new Error('response lost')) : Promise.resolve(snapshot()));
        vi.stubGlobal('fetch', fetch);
        const { result } = renderHook(() => useAiAssistantChat({ deskId: 'desk-1', connected: true, subscribe, sendMessage }));
        await waitFor(() => expect(result.current.permissionRequests).toHaveLength(1));
        const before = fetch.mock.calls.length;
        await act(async () => expect(await result.current.decidePermission(request, true)).toBe(false));
        expect(fetch.mock.calls.slice(before).map(([url]) => String(url).includes('permission-decision'))).toEqual([true, false]);
        expect(result.current.permissionUpdating).toBe(false);
    });
});
