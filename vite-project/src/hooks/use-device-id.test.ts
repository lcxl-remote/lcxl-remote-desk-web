import { describe, it, expect, vi } from "vitest"
import { renderHook } from "@testing-library/react"

// Mutable connection list backing the mocked query hook.
const h = vi.hoisted(() => ({
    connections: undefined as unknown[] | undefined,
}))
vi.mock("@/services/hooks/connectionController/useListConnections", () => ({
    useListConnections: () => ({ data: h.connections }),
}))

import { useDeviceId } from "./use-device-id"

describe("useDeviceId", () => {
    it("resolves the manager device id for a connection", () => {
        h.connections = [
            { connection_id: "conn-1", device_id: "42" },
            { connection_id: "conn-2", device_id: "7" },
        ]
        const { result } = renderHook(() => useDeviceId("conn-2"))
        expect(result.current).toBe("7")
    })

    it("returns undefined for an OSS connection with no device id", () => {
        // The OSS single-instance signal leaves device_id unset; callers then
        // fall back to routing by connection_id (dual-target wire model).
        h.connections = [{ connection_id: "conn-1" }]
        const { result } = renderHook(() => useDeviceId("conn-1"))
        expect(result.current).toBeUndefined()
    })

    it("returns undefined when the connection is unknown", () => {
        h.connections = [{ connection_id: "conn-1", device_id: "42" }]
        const { result } = renderHook(() => useDeviceId("missing"))
        expect(result.current).toBeUndefined()
    })

    it("returns undefined when no connection id is given", () => {
        h.connections = [{ connection_id: "conn-1", device_id: "42" }]
        const { result } = renderHook(() => useDeviceId(undefined))
        expect(result.current).toBeUndefined()
    })

    it("tolerates a not-yet-loaded connection list", () => {
        h.connections = undefined
        const { result } = renderHook(() => useDeviceId("conn-1"))
        expect(result.current).toBeUndefined()
    })
})
