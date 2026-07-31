export const ADMISSION_RETRY_INTERVALS_MS = [2_000, 5_000, 10_000, 20_000] as const;

export class AdmissionRetrySchedule {
    private index = 0;

    reset(): void {
        this.index = 0;
    }

    nextDelay(): number | null {
        const delay = ADMISSION_RETRY_INTERVALS_MS[this.index];
        if (delay === undefined) return null;
        this.index += 1;
        return delay;
    }
}
