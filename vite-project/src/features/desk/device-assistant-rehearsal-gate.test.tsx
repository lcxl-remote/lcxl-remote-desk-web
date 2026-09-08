import { act, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { DeviceAssistantRehearsalGate } from './device-assistant-rehearsal-gate';
import type { SendTrackedOptions, SignalingSubscriber } from './use-desk-signaling';
import { deskErrorCodeEnum } from '@/services/types';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
const transport = vi.hoisted(() => ({ subscriber: (() => {}) as SignalingSubscriber, send: vi.fn(), cancel: vi.fn() }));
function subscribe(callback: SignalingSubscriber) { transport.subscriber = callback; return () => {}; }
vi.mock('./use-desk-signaling', () => ({ useDeskSignaling: () => ({ isConnected: true, subscribe, sendTracked: transport.send, cancelQueued: transport.cancel }) }));
const row = { initial_message_id: 'rehearsal:run-1:input', rehearsal_id: 'run-1', target_device_id: 'device', client_conversation_id: 'rehearsal_fresh', prompt: 'Frozen task', status: 'pending' };
function respond(index: number, rehearsal = row) {
    const request = transport.send.mock.calls[index][0] as SendTrackedOptions;
    transport.subscriber({ signaling_type: 646, request_id: request.requestId, response_state: { error_code: deskErrorCodeEnum.SUCCESS }, signaling_data: { result: 'rehearsal', task: {}, rehearsal } });
}
beforeEach(() => {
    transport.send.mockReset();
    transport.send.mockImplementation((request: SendTrackedOptions) => ({ requestId: request.requestId!, disposition: 'sent' }));
});

describe('guided run workspace admission', () => {
    it('mounts the workspace only after a matching server record arrives', async () => {
        const workspace = vi.fn(rehearsal => <p>{rehearsal.prompt}</p>);
        render(<DeviceAssistantRehearsalGate rehearsalId="run-1" deviceId="device">{workspace}</DeviceAssistantRehearsalGate>);
        await waitFor(() => expect(transport.send).toHaveBeenCalledTimes(1));
        expect(workspace).not.toHaveBeenCalled();
        expect(transport.send.mock.calls[0][0].data).toEqual({ operation: 'get_rehearsal', rehearsal_id: 'run-1' });
        await act(async () => respond(0));
        expect(screen.getByText('Frozen task')).toBeInTheDocument();
        expect(transport.send).toHaveBeenCalledTimes(1);
    });
    it.each([
        { ...row, target_device_id: 'another-device' },
        { ...row, rehearsal_id: 'another-run' },
        { ...row, client_conversation_id: 'ordinary-chat' },
        { ...row, initial_message_id: 'ordinary-input' },
    ])('rejects a mismatched or ordinary conversation record', async rehearsal => {
        const workspace = vi.fn(() => <p>Workspace</p>);
        render(<DeviceAssistantRehearsalGate rehearsalId="run-1" deviceId="device">{workspace}</DeviceAssistantRehearsalGate>);
        await act(async () => respond(0, rehearsal));
        expect(screen.getByRole('alert')).toBeInTheDocument();
        expect(workspace).not.toHaveBeenCalled();
    });
    it('unmounts the admitted workspace when its signaling client changes', async () => {
        const workspace = vi.fn(rehearsal => <p>{rehearsal.prompt}</p>);
        const view = render(<DeviceAssistantRehearsalGate rehearsalId="run-1" deviceId="device">{workspace}</DeviceAssistantRehearsalGate>);
        await act(async () => respond(0));
        expect(screen.getByText('Frozen task')).toBeInTheDocument();
        workspace.mockClear();
        transport.cancel = vi.fn();
        view.rerender(<DeviceAssistantRehearsalGate rehearsalId="run-1" deviceId="device">{workspace}</DeviceAssistantRehearsalGate>);
        expect(screen.queryByText('Frozen task')).not.toBeInTheDocument();
        expect(workspace).not.toHaveBeenCalled();
        await act(async () => respond(1));
        expect(screen.getByText('Frozen task')).toBeInTheDocument();
    });
    it('ignores an earlier lookup after the launch parameter changes', async () => {
        const workspace = vi.fn(rehearsal => <p>{rehearsal.prompt}</p>);
        const view = render(<DeviceAssistantRehearsalGate rehearsalId="run-1" deviceId="device">{workspace}</DeviceAssistantRehearsalGate>);
        view.rerender(<DeviceAssistantRehearsalGate rehearsalId="run-2" deviceId="device">{workspace}</DeviceAssistantRehearsalGate>);
        await act(async () => respond(0));
        expect(workspace).not.toHaveBeenCalled();
        await act(async () => respond(1, { ...row, rehearsal_id: 'run-2', initial_message_id: 'rehearsal:run-2:input', prompt: 'Current task' }));
        expect(screen.getByText('Current task')).toBeInTheDocument();
        expect(screen.queryByText('Frozen task')).not.toBeInTheDocument();
    });
});
