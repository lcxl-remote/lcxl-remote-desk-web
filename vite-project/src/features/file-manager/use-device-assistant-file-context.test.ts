import { act, cleanup, renderHook } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useDeviceAssistantFileContext, type AssistantFileObjectRef } from './use-device-assistant-file-context';
import { SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_OBJECT_CONTEXT_UPDATED } from '@/features/desk/constants';
import type { SignalingSubscriber } from '@/features/desk/use-desk-signaling';

const signaling = vi.hoisted(() => ({
    subscribers: new Set<SignalingSubscriber>(),
    sendMessage: vi.fn(),
    subscribe: vi.fn(),
}));
vi.mock('@/features/desk/use-desk-signaling', () => ({ useDeskSignaling: () => ({
    isConnected: true, sendMessage: signaling.sendMessage, subscribe: signaling.subscribe,
}) }));
const reference: AssistantFileObjectRef = {
    token: 'opaque-token', snapshot_id: 'snapshot', object_kind: 'directory', expires_at: '2099-01-01T00:00:00Z',
};
function ack(index = 0, overrides = {}) {
    const [, data, , id] = signaling.sendMessage.mock.calls[index];
    const requestId = id ?? signaling.sendMessage.mock.results[index].value;
    for (const receive of signaling.subscribers) receive({
        signaling_type: SIGNALING_TYPE_CODE_DEVICE_ASSISTANT_OBJECT_CONTEXT_UPDATED,
        request_id: requestId,
        signaling_data: { conversation_id: data.conversation_id, client_request_id: data.client_request_id,
            changed: true, error: null, ...overrides },
    });
}
beforeEach(() => {
    vi.useFakeTimers(); localStorage.clear(); signaling.subscribers.clear();
    signaling.sendMessage.mockReset().mockImplementation((_type, _data, _target, id) => id ?? crypto.randomUUID());
    signaling.subscribe.mockReset().mockImplementation((receive: SignalingSubscriber) => {
        signaling.subscribers.add(receive);
        return () => signaling.subscribers.delete(receive);
    });
});
afterEach(() => { cleanup(); vi.useRealTimers(); });

describe.each([
    ['Windows', 'C:\\用户资料\\季度 报告'],
    ['macOS', '/Users/owner/季度 报告'],
])('assistant file context on %s', (_platform, path) => {
    it('attaches an opaque reference to the existing device conversation and awaits its receipt', () => {
        localStorage.setItem('device-assistant-conversation:device-A', 'conversation-A');
        const { result } = renderHook(() => useDeviceAssistantFileContext('connection-A', 'device-A'));
        act(() => { expect(result.current.attach(path, 'Selected directory', reference)).toBe(true); });
        expect(signaling.sendMessage.mock.calls[0][1]).toMatchObject({
            conversation_id: 'conversation-A', operation: { kind: 'attach_file', object_ref: reference },
        });
        expect(signaling.sendMessage.mock.calls[0][2]).toBe('connection-A');
        expect(result.current.pendingPath).toBe(path);
        act(() => { expect(result.current.attach(path, 'Duplicate', reference)).toBe(false); });
        expect(signaling.sendMessage).toHaveBeenCalledTimes(1);
        act(() => ack());
        expect(result.current.lastAddedPath).toBe(path);
        expect(result.current.pendingPath).toBeNull();
    });

    it.each(['connection', 'conversation scope'])('ignores the old receipt after changing %s', changed => {
        const { result, rerender } = renderHook(({ connection, scope }) =>
            useDeviceAssistantFileContext(connection, scope),
        { initialProps: { connection: 'connection-A', scope: 'device-A' } });
        act(() => { result.current.attach(path, 'Old selection', reference); });
        rerender({ connection: changed === 'connection' ? 'connection-B' : 'connection-A',
            scope: changed === 'connection' ? 'device-A' : 'device-B' });
        expect(result.current.pendingPath).toBeNull();
        act(() => ack());
        expect(result.current.lastAddedPath).toBeNull();
        act(() => { expect(result.current.attach(path, 'New selection', reference)).toBe(true); });
        act(() => ack(0));
        expect(result.current.pendingPath).toBe(path);
        act(() => ack(1));
        expect(result.current.lastAddedPath).toBe(path);
        act(() => vi.advanceTimersByTime(10_001));
        expect(result.current.error).toBeNull();
    });

    it.each(['conversation_id', 'client_request_id'])('ignores a mismatched %s without claiming success', field => {
        const { result } = renderHook(() => useDeviceAssistantFileContext('connection-A', 'device-A'));
        act(() => { result.current.attach(path, 'Selected directory', reference); });
        act(() => ack(0, { [field]: 'unrelated' }));
        expect(result.current.lastAddedPath).toBeNull();
        expect(result.current.pendingPath).toBe(path);
        act(() => ack());
        expect(result.current.lastAddedPath).toBe(path);
    });

    it('keeps rejection separate from success and never replays a timed-out attachment', () => {
        const { result } = renderHook(() => useDeviceAssistantFileContext('connection-A', 'device-A'));
        act(() => { result.current.attach(path, 'Selected directory', reference); });
        act(() => ack(0, { changed: false, error: 'reference expired' }));
        expect(result.current.lastAddedPath).toBeNull();
        expect(result.current.error).toBe('reference expired');
        act(() => { result.current.attach(path, 'New reference', { ...reference, token: 'new-token' }); });
        act(() => vi.advanceTimersByTime(10_001));
        expect(result.current.error).toBe('timeout');
        expect(result.current.pendingPath).toBeNull();
        act(() => ack(1));
        expect(result.current.lastAddedPath).toBeNull();
        expect(result.current.error).toBe('timeout');
        expect(signaling.sendMessage).toHaveBeenCalledTimes(2);
    });
});
