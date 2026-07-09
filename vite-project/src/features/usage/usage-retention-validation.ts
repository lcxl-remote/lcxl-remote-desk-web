/** Inclusive bounds the backend enforces for a retention window, in days. */
export const MIN_RETENTION_DAYS = 1;
export const MAX_RETENTION_DAYS = 10000;

/**
 * Whether a retention window (in days) is an integer within the accepted range.
 * Kept pure so the client-side guard on the retention editor is unit-testable
 * without rendering; the backend re-validates the same bounds authoritatively.
 */
export function isValidRetentionDays(days: number): boolean {
    return Number.isInteger(days) && days >= MIN_RETENTION_DAYS && days <= MAX_RETENTION_DAYS;
}
