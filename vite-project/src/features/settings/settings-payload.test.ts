import { describe, it, expect } from "vitest"
import type { SystemSettings } from "@/services/types"
import { mergeSystemSettings } from "@/features/settings/settings-payload"

// Full snapshot as returned by `query_settings`, including fields owned by other
// settings pages and config-only fields not rendered in any form.
const fullSettings: SystemSettings = {
    enable_ipv6: true,
    port: 8081,
    listen_addr_ipv4: "0.0.0.0",
    listen_addr_ipv6: "::",
    telemetry_consent: true,
    auto_start: true,
    signaling_url: "ws://remote/api/desk/signaling",
    signaling_token: "sig-token",
    manager_url: "ws://manager/api/desk/signaling",
    manager_api_token: "mgr-token",
    local_signaling_token: "local-token",
    worker_heartbeat_timeout_secs: 30,
    webrtc_ice_failed_timeout_secs: 12,
} as SystemSettings

describe("mergeSystemSettings", () => {
    it("preserves fields owned by other pages when the outbound page saves", () => {
        // Outbound (Desk-connection) page only edits the four outbound fields.
        const edits: Partial<SystemSettings> = {
            signaling_url: "ws://new-remote/api/desk/signaling",
            signaling_token: "new-sig-token",
            manager_url: null,
            manager_api_token: null,
        }

        const payload = mergeSystemSettings(fullSettings, edits)

        // Edited fields take effect.
        expect(payload.signaling_url).toBe("ws://new-remote/api/desk/signaling")
        expect(payload.manager_url).toBeNull()
        // System-page fields survive untouched (guards against B3 full-replace wipe).
        expect(payload.port).toBe(8081)
        expect(payload.listen_addr_ipv4).toBe("0.0.0.0")
        expect(payload.enable_ipv6).toBe(true)
        // Config-only and internal fields survive too.
        expect(payload.worker_heartbeat_timeout_secs).toBe(30)
        expect(payload.local_signaling_token).toBe("local-token")
    })

    it("preserves outbound fields when the system page saves", () => {
        // System page only edits general fields, never the outbound ones.
        const edits: Partial<SystemSettings> = {
            enable_ipv6: false,
            port: 9000,
            listen_addr_ipv4: "127.0.0.1",
            listen_addr_ipv6: "::1",
            telemetry_consent: false,
            auto_start: false,
        }

        const payload = mergeSystemSettings(fullSettings, edits)

        expect(payload.port).toBe(9000)
        expect(payload.enable_ipv6).toBe(false)
        // Outbound fields owned by the Desk-connection page are not wiped.
        expect(payload.signaling_url).toBe("ws://remote/api/desk/signaling")
        expect(payload.manager_api_token).toBe("mgr-token")
        expect(payload.local_signaling_token).toBe("local-token")
    })
})
