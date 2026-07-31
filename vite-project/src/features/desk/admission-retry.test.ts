import { describe, expect, it } from 'vitest';

import { ADMISSION_RETRY_INTERVALS_MS, AdmissionRetrySchedule } from './admission-retry';

describe('AdmissionRetrySchedule', () => {
    it('uses adjacent intervals whose total covers the host proof recovery window', () => {
        const schedule = new AdmissionRetrySchedule();
        expect([
            schedule.nextDelay(),
            schedule.nextDelay(),
            schedule.nextDelay(),
            schedule.nextDelay(),
        ]).toEqual([...ADMISSION_RETRY_INTERVALS_MS]);
        expect(schedule.nextDelay()).toBeNull();
        expect(ADMISSION_RETRY_INTERVALS_MS.reduce((sum, delay) => sum + delay, 0)).toBe(37_000);
    });

    it('can be reset for a new logical attempt', () => {
        const schedule = new AdmissionRetrySchedule();
        expect(schedule.nextDelay()).toBe(2_000);
        expect(schedule.nextDelay()).toBe(5_000);
        schedule.reset();
        expect(schedule.nextDelay()).toBe(2_000);
    });
});
