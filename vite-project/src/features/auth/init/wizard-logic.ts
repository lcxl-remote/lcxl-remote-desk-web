// Pure logic for the onboarding wizard, extracted so the Step 2 gate decision
// and the final security payload can be unit tested without rendering the form.

import type { ConnectionVerifyResult, SecuritySettings } from "@/services/types"

export const SECURITY_CAPABILITIES = [
    "allow_remote_control",
    "allow_clipboard_sync",
    "allow_private_screen",
    "allow_whiteboard",
    "allow_terminal",
    "allow_file_browse",
    "allow_file_transfer",
] as const

export type SecurityCapability = (typeof SECURITY_CAPABILITIES)[number]
export type SecurityToggles = Record<SecurityCapability, boolean>

/** Whether the manager step has both a resolved URL and a token (gate for Next). */
export function isManagerConfigured(url: string, token: string): boolean {
    return url.trim().length > 0 && token.trim().length > 0
}

/**
 * Decide what a Step 2 "next" verify result means:
 *   - "advance": authenticated and (for manager) the console is up
 *   - "token": reachable but the token was rejected
 *   - "unreachable": could not reach the target
 */
export function managerNextDecision(
    result: ConnectionVerifyResult | null | undefined,
): "advance" | "token" | "unreachable" {
    if (result?.auth_ok && result.console_ok !== false) return "advance"
    if (result?.reached) return "token"
    return "unreachable"
}

/**
 * Build the `SecuritySettings` payload from the per-capability toggles: ON =
 * auto-allow (`true`), OFF = prompt each time (`null`). `approval_timeout` is left
 * `null`; the backend normalizes an unset timeout to its 30s default.
 */
export function buildSecurityPayload(toggles: SecurityToggles): SecuritySettings {
    const payload: Record<string, boolean | null> = {}
    for (const cap of SECURITY_CAPABILITIES) {
        payload[cap] = toggles[cap] ? true : null
    }
    payload.approval_timeout = null
    return payload as SecuritySettings
}
