import { describe, expect, it } from 'vitest';
import { deskErrorCodeEnum } from '@/services/types';
import { ScheduleRequestError, scheduleErrorMessage } from './client';

const t = (key: string, options?: Record<string, unknown>) =>
    options ? `${key}:${JSON.stringify(options)}` : key;

describe('schedule error messages', () => {
    it('states the accepted continuation window when retention is exceeded', () => {
        const error = new ScheduleRequestError('server', 'raw', deskErrorCodeEnum.SCHEDULE_EXCEEDS_SESSION_RETENTION, 2_584_800);
        expect(scheduleErrorMessage(error, t)).toBe('schedules.retentionExceeded:{"days":29,"hours":22}');
    });
    it('keeps server messages and localizes transport failures', () => {
        expect(scheduleErrorMessage(new ScheduleRequestError('server', 'task changed'), t)).toBe('task changed');
        expect(scheduleErrorMessage(new ScheduleRequestError('timeout'), t)).toBe('schedules.requestFailed');
        expect(scheduleErrorMessage(new Error('x'), t, 'fallback')).toBe('fallback');
    });
});
