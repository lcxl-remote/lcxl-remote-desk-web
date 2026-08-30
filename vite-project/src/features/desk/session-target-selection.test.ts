import { describe, expect, it } from "vitest"

import { parseSessionTargetList } from "./session-target-selection"

describe("parseSessionTargetList", () => {
    it("accepts the daemon's safe target projection", () => {
        expect(parseSessionTargetList({
            revision: 4,
            targets: [{
                target_id: "opaque-id",
                display_name: "alice · seat0",
                session_type: "wayland",
                seat: "seat0",
                foreground: false,
                remote_desktop_ready: true,
                terminal_ready: true,
                file_ready: true,
                assistant_ready: true,
            }],
        })?.targets[0].target_id).toBe("opaque-id")
    })

    it("rejects malformed candidates instead of guessing a target", () => {
        expect(parseSessionTargetList({
            revision: 4,
            targets: [{ display_name: "missing id" }],
        })).toBeNull()
    })
})
