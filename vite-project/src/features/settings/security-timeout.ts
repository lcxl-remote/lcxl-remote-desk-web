// Approval-timeout select <-> stored-value mapping, extracted so it can be unit
// tested without rendering the settings form.
//
// Semantics (mirrors the backend `SecuritySettings`):
//   - "never" is persisted as the present value 0 (not null), so it survives a
//     save/reload round-trip instead of being resurrected as the default.
//   - a missing/null value renders as the 30s default rather than "never".

// Default approval timeout in seconds, mirrored from the backend default.
export const DEFAULT_APPROVAL_TIMEOUT = 30

/** Map a stored timeout (seconds) to the select's string value. */
export function mapTimeoutToSelectValue(val: number | null | undefined): string {
    // A present 0 means "never"; only a missing value falls back to the default.
    return (val ?? DEFAULT_APPROVAL_TIMEOUT).toString()
}

/** Map the select's string value back to a stored timeout (seconds). */
export function mapTimeoutFromSelectValue(val: string): number {
    // "0" (never) must round-trip as the present value 0, never null.
    const num = parseInt(val, 10)
    return Number.isFinite(num) && num > 0 ? num : 0
}
