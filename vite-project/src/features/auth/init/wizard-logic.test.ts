import { describe, expect, it } from "vitest"
import type { ConnectionVerifyResult } from "@/services/types"
import {
    CONNECTION_INSECURE_TRANSPORT,
    SECURITY_CAPABILITIES,
    buildSecurityPayload,
    isInsecureConnection,
    isInsecureTransportRefused,
    isManagerConfigured,
    managerNextDecision,
    type SecurityToggles,
} from "./wizard-logic"

const allOff: SecurityToggles = {
    allow_remote_control: false,
    allow_clipboard_sync: false,
    allow_private_screen: false,
    allow_whiteboard: false,
    allow_terminal: false,
    allow_file_browse: false,
    allow_file_delete: false,
    allow_file_transfer: false,
}

function result(partial: Partial<ConnectionVerifyResult>): ConnectionVerifyResult {
    return {
        ok: false,
        reached: false,
        auth_ok: false,
        secure: false,
        error_code: 0,
        message: "",
        ...partial,
    }
}

describe("isManagerConfigured (Step 2 gate)", () => {
    it("requires both a resolved URL and a token", () => {
        expect(isManagerConfigured("wss://a/x", "tok")).toBe(true)
        expect(isManagerConfigured("", "tok")).toBe(false)
        expect(isManagerConfigured("wss://a/x", "")).toBe(false)
        expect(isManagerConfigured("   ", "  ")).toBe(false)
    })
})

describe("managerNextDecision", () => {
    it("advances only when authenticated and the console is not down", () => {
        expect(managerNextDecision(result({ auth_ok: true, reached: true, console_ok: true }))).toBe("advance")
        // console_ok null/undefined counts as ok (signaling target has no console).
        expect(managerNextDecision(result({ auth_ok: true, reached: true, console_ok: null }))).toBe("advance")
    })

    it("reports a token problem when reachable but not authenticated", () => {
        expect(managerNextDecision(result({ auth_ok: false, reached: true }))).toBe("token")
    })

    it("blocks advancing when the manager console is down even if auth passed", () => {
        expect(managerNextDecision(result({ auth_ok: true, reached: true, console_ok: false }))).toBe("token")
    })

    it("reports unreachable when nothing came back or on transport failure", () => {
        expect(managerNextDecision(result({ reached: false }))).toBe("unreachable")
        expect(managerNextDecision(null)).toBe("unreachable")
        expect(managerNextDecision(undefined)).toBe("unreachable")
    })
})

describe("isInsecureConnection", () => {
    it("flags a reached target that answered only over plaintext", () => {
        expect(isInsecureConnection(result({ reached: true, secure: false }))).toBe(true)
    })

    it("is not insecure when the connection is TLS-encrypted", () => {
        expect(isInsecureConnection(result({ reached: true, secure: true }))).toBe(false)
    })

    it("is not insecure when the target was never reached", () => {
        expect(isInsecureConnection(result({ reached: false, secure: false }))).toBe(false)
        expect(isInsecureConnection(null)).toBe(false)
        expect(isInsecureConnection(undefined)).toBe(false)
    })
})

describe("isInsecureTransportRefused", () => {
    it("flags a public-plaintext refusal by its error code", () => {
        expect(
            isInsecureTransportRefused(
                result({ reached: false, error_code: CONNECTION_INSECURE_TRANSPORT }),
            ),
        ).toBe(true)
    })

    it("is not a refusal for a reachable-but-plaintext downgrade or other codes", () => {
        // A soft insecure warning (reached over plaintext) is NOT a hard refusal.
        expect(isInsecureTransportRefused(result({ reached: true, secure: false }))).toBe(false)
        expect(isInsecureTransportRefused(result({ error_code: 64 }))).toBe(false)
        expect(isInsecureTransportRefused(null)).toBe(false)
        expect(isInsecureTransportRefused(undefined)).toBe(false)
    })
})

describe("buildSecurityPayload", () => {
    it("maps ON to true and OFF to null, with an unset timeout", () => {
        const payload = buildSecurityPayload({ ...allOff, allow_terminal: true, allow_remote_control: true })
        expect(payload.allow_terminal).toBe(true)
        expect(payload.allow_remote_control).toBe(true)
        expect(payload.allow_clipboard_sync).toBeNull()
        // Backend normalizes an unset timeout to the 30s default.
        expect(payload.approval_timeout).toBeNull()
    })

    it("emits null for every capability when all toggles are off", () => {
        const payload = buildSecurityPayload(allOff)
        for (const cap of SECURITY_CAPABILITIES) {
            expect(payload[cap]).toBeNull()
        }
    })
})
