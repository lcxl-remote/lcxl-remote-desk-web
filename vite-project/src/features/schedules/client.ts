import { v4 } from 'uuid';
import type { ScheduleManagementRequest, ScheduleManagementResponse } from '@/services/types';
import { deskErrorCodeEnum } from '@/services/types';
import type { SendTrackedOptions, SendTrackedResult, SignalingMessage } from '@/features/desk/use-desk-signaling';

export const MANAGE_SCHEDULES = 645;
export const SCHEDULES_MANAGED = 646;
const expectedResults: Record<ScheduleManagementRequest['operation'], ScheduleManagementResponse['result']> = {
    list_resume_sources: 'resume_sources',
    revoke_task_authorization: 'task',
    set_failure_threshold: 'task',
    change_prompt: 'task',
    convert_time: 'converted_time',
    list: 'list',
    search: 'search_results',
    list_runs: 'runs',
    get: 'task',
    decide_run_directory: 'task',
    revoke_run_directory: 'task',
    acknowledge_run_outcome: 'task',
    dispose_run_outcome: 'task',
    resume_task: 'task',
    run_task_now: 'task',
    cancel_task_run: 'task',
    get_task_contract: 'task_contract',
    generate_task_contract: 'task_contract',
    save_task_contract: 'task_contract',
    publish_task: 'task',
    create_draft: 'task',
    activate_conversation_resume: 'task',
    reserve_rehearsal: 'rehearsal',
    get_rehearsal: 'rehearsal',
    get_task_rehearsal: 'task_rehearsal',
    get_rehearsal_permissions: 'rehearsal_permissions',
    cancel_pending_rehearsal: 'rehearsal',
    rename: 'task',
    change_time: 'task',
    pause: 'task',
    delete: 'task',
};
export class ScheduleRequestError extends Error {
    readonly reason: 'offline' | 'timeout' | 'closed' | 'invalid' | 'server';
    constructor(reason: ScheduleRequestError['reason'], message?: string) {
        super(message ?? reason);
        this.reason = reason;
    }
}

type Pending = { resolve: (value: ScheduleManagementResponse) => void; reject: (error: Error) => void; timer: ReturnType<typeof setTimeout>; result: string };
export class ScheduleClient {
    private pending = new Map<string, Pending>();
    private send: (options: SendTrackedOptions) => SendTrackedResult;
    private cancel: (key: string) => void;
    constructor(send: (options: SendTrackedOptions) => SendTrackedResult, cancel: (key: string) => void) { this.send = send; this.cancel = cancel; }
    receive = (message: SignalingMessage) => {
        if (message.signaling_type !== SCHEDULES_MANAGED || message.from_connection_id || !message.request_id) return;
        const pending = this.pending.get(message.request_id);
        if (!pending) return;
        this.pending.delete(message.request_id);
        clearTimeout(pending.timer);
        if (!message.response_state || message.response_state.error_code !== deskErrorCodeEnum.SUCCESS) {
            pending.reject(new ScheduleRequestError('server', message.response_state?.message ?? undefined));
        } else if (message.signaling_data?.result !== pending.result) {
            pending.reject(new ScheduleRequestError('invalid'));
        } else {
            pending.resolve(message.signaling_data as ScheduleManagementResponse);
        }
    };
    request(request: ScheduleManagementRequest): Promise<ScheduleManagementResponse> {
        const id = v4();
        return new Promise((resolve, reject) => {
            const timer = setTimeout(() => {
                this.cancel(id);
                this.pending.delete(id);
                reject(new ScheduleRequestError('timeout'));
            }, 15_000);
            this.pending.set(id, { resolve, reject, timer, result: expectedResults[request.operation] });
            const sent = this.send({ type: MANAGE_SCHEDULES, data: request, requestId: id, replaceKey: id });
            if (sent.disposition === 'queued') {
                this.cancel(id);
                this.pending.delete(id);
                clearTimeout(timer);
                reject(new ScheduleRequestError('offline'));
            }
        });
    }
    close() {
        for (const [id, pending] of this.pending) {
            this.cancel(id);
            clearTimeout(pending.timer);
            pending.reject(new ScheduleRequestError('closed'));
        }
        this.pending.clear();
    }
}
