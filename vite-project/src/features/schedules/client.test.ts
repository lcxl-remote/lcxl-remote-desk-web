import { afterEach, describe, expect, it, vi } from 'vitest';
import { ScheduleClient, MANAGE_SCHEDULES, SCHEDULES_MANAGED } from './client';
import type { SendTrackedOptions } from '@/features/desk/use-desk-signaling';
import { deskErrorCodeEnum, type ScheduleManagementRequest } from '@/services/types';

describe('schedule request transport', () => {
    afterEach(() => vi.useRealTimers());
    it('requires a contract review response instead of treating a task response as authorization', async () => {
        vi.useFakeTimers();
        let sent!: SendTrackedOptions;
        const client = new ScheduleClient(options => { sent = options; return { requestId: options.requestId!, disposition: 'sent' }; }, vi.fn());
        const pending = client.request({ operation: 'get_task_contract', schedule_id: 'task-1' });
        client.receive({ signaling_type: SCHEDULES_MANAGED, request_id: sent.requestId, signaling_data: { result: 'task_contract', task: {}, task_revision: 1, prompt_sha256: 'a'.repeat(64), contract: null, contract_sha256: null }, response_state: { error_code: deskErrorCodeEnum.SUCCESS } });
        await expect(pending).resolves.toMatchObject({ result: 'task_contract', contract: null });
        const wrong = client.request({ operation: 'get_task_contract', schedule_id: 'task-1' });
        client.receive({ signaling_type: SCHEDULES_MANAGED, request_id: sent.requestId, signaling_data: { result: 'task' }, response_state: { error_code: deskErrorCodeEnum.SUCCESS } });
        await expect(wrong).rejects.toMatchObject({ reason: 'invalid' });
        expect(vi.getTimerCount()).toBe(0);
    });
    it('matches the central response by id and type, ignoring peer forgeries', async () => {
        vi.useFakeTimers();
        let sent!: SendTrackedOptions;
        const client = new ScheduleClient(options => { sent = options; return { requestId: options.requestId!, disposition: 'sent' }; }, vi.fn());
        const request = client.request({ operation: 'list', after: null, limit: 20 });
        expect(sent.type).toBe(MANAGE_SCHEDULES);
        expect(sent.toConnectionId).toBeUndefined();
        const response = { signaling_type: SCHEDULES_MANAGED, request_id: sent.requestId, signaling_data: { result: 'list', tasks: [], next_cursor: null }, response_state: { error_code: deskErrorCodeEnum.SUCCESS } };
        let settled = false;
        void request.then(() => { settled = true; });
        client.receive({ ...response, from_connection_id: 'peer' });
        client.receive({ ...response, request_id: 'wrong' });
        client.receive({ ...response, signaling_type: 634 });
        await Promise.resolve();
        expect(settled).toBe(false);
        client.receive(response);
        await expect(request).resolves.toMatchObject({ result: 'list', tasks: [] });
    });
    it('expects a rehearsal response for reserve, query, and cancellation', async () => {
        vi.useFakeTimers();
        const requests: ScheduleManagementRequest[] = [
            { operation: 'reserve_rehearsal', schedule_id: 'task-1', expected_revision: 1, client_request_key: 'reserve-1' },
            { operation: 'get_rehearsal', rehearsal_id: 'rehearsal-1' },
            { operation: 'cancel_pending_rehearsal', rehearsal_id: 'rehearsal-1', expected_revision: 2 },
        ];
        let sent!: SendTrackedOptions;
        const client = new ScheduleClient(options => { sent = options; return { requestId: options.requestId!, disposition: 'sent' }; }, vi.fn());
        for (const payload of requests) {
            const pending = client.request(payload);
            client.receive({ signaling_type: SCHEDULES_MANAGED, request_id: sent.requestId, signaling_data: { result: 'rehearsal', task: {}, rehearsal: { rehearsal_id: 'rehearsal-1' } }, response_state: { error_code: deskErrorCodeEnum.SUCCESS } });
            await expect(pending).resolves.toMatchObject({ result: 'rehearsal', rehearsal: { rehearsal_id: 'rehearsal-1' } });
        }
        const wrongType = client.request(requests[0]);
        client.receive({ signaling_type: SCHEDULES_MANAGED, request_id: sent.requestId, signaling_data: { result: 'task', task: {} }, response_state: { error_code: deskErrorCodeEnum.SUCCESS } });
        await expect(wrongType).rejects.toMatchObject({ reason: 'invalid' });
        expect(vi.getTimerCount()).toBe(0);
    });
    it('receives the permission review separately from rehearsal status', async () => {
        vi.useFakeTimers();
        let sent!: SendTrackedOptions;
        const client = new ScheduleClient(options => { sent = options; return { requestId: options.requestId!, disposition: 'sent' }; }, vi.fn());
        const payload: ScheduleManagementRequest = { operation: 'get_rehearsal_permissions', rehearsal_id: 'rehearsal-1' };
        const request = client.request(payload);
        client.receive({ signaling_type: SCHEDULES_MANAGED, request_id: sent.requestId, signaling_data: {
            result: 'rehearsal_permissions', rehearsal_id: 'rehearsal-1', observations: [], unconfirmed_tool_call_ids: ['failed-read'], unclassified_tool_call_ids: ['other-tool'],
        }, response_state: { error_code: deskErrorCodeEnum.SUCCESS } });
        await expect(request).resolves.toMatchObject({ result: 'rehearsal_permissions', unconfirmed_tool_call_ids: ['failed-read'], unclassified_tool_call_ids: ['other-tool'] });
        const wrongType = client.request(payload);
        client.receive({ signaling_type: SCHEDULES_MANAGED, request_id: sent.requestId, signaling_data: { result: 'rehearsal' }, response_state: { error_code: deskErrorCodeEnum.SUCCESS } });
        await expect(wrongType).rejects.toMatchObject({ reason: 'invalid' });
        expect(vi.getTimerCount()).toBe(0);
    });
    it('cancels unsent messages so reconnect cannot replay an old mutation', async () => {
        vi.useFakeTimers();
        const cancel = vi.fn();
        const send = vi.fn((options: SendTrackedOptions) => ({ requestId: options.requestId!, disposition: 'queued' as const }));
        const client = new ScheduleClient(send, cancel);
        await expect(client.request({ operation: 'delete', schedule_id: 'task-1', expected_revision: 2 })).rejects.toMatchObject({ reason: 'offline' });
        expect(cancel).toHaveBeenCalledWith(send.mock.calls[0][0].requestId);
        expect(vi.getTimerCount()).toBe(0);
    });
    it('timeouts do not retry, and closing releases every pending request', async () => {
        vi.useFakeTimers();
        const send = vi.fn((options: SendTrackedOptions) => ({ requestId: options.requestId!, disposition: 'sent' as const }));
        const client = new ScheduleClient(send, vi.fn());
        const timeout = expect(client.request({ operation: 'pause', schedule_id: 'task-1', expected_revision: 2 })).rejects.toMatchObject({ reason: 'timeout' });
        await vi.advanceTimersByTimeAsync(15_000);
        await timeout;
        expect(send).toHaveBeenCalledTimes(1);
        const closed = expect(client.request({ operation: 'list', after: null, limit: 20 })).rejects.toMatchObject({ reason: 'closed' });
        client.close();
        await closed;
        expect(vi.getTimerCount()).toBe(0);
    });
});
