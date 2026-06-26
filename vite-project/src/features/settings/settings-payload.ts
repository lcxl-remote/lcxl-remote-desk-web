import type { SystemSettings } from "@/services/types"

/**
 * Build a full `SystemSettings` payload for `update_settings`.
 *
 * The backend `update_settings` endpoint is a full-struct replace: any field
 * absent from the payload is reset (only a few auto-generated secrets are
 * carried over server-side). The settings UI is split across several pages that
 * each edit a subset of fields, so every page must submit the COMPLETE struct.
 * This layers a page's edited fields on top of the latest complete settings so
 * fields owned by other pages (and config-only fields such as
 * `worker_heartbeat_*` / `webrtc_ice_*`) are preserved.
 */
export function mergeSystemSettings(
    base: Partial<SystemSettings>,
    edits: Partial<SystemSettings>,
): SystemSettings {
    return { ...base, ...edits } as SystemSettings
}
