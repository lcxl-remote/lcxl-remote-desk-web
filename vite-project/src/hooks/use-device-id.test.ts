import { describe, it, expect, vi } from "vitest"
import { renderHook } from "@testing-library/react"

// Mutable connection list backing the mocked query hook.
const h = vi.hoisted(() => ({
    connections: undefined as unknown[] | undefined,
    isLoading: false,
}))
vi.mock("@/services/hooks/connectionController/useListConnections", () => ({
    useListConnections: () => ({
        data: h.connections,
        isLoading: h.isLoading,
    }),
}))

import {
    useDeviceConnection,
    useDeviceConnectionResolution,
    useDeviceId,
} from "./use-device-id"

describe("useDeviceId", () => {
    it("exposes the host-reported platform with the connection metadata", () => {
        h.connections = [{
            connection_id: "conn-1",
            device_id: "42",
            version_info: { operation_system: "Mac" },
        }]
        const { result } = renderHook(() => useDeviceConnection("conn-1"))
        expect(result.current?.version_info.operation_system).toBe("Mac")
    })

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

describe("useDeviceConnectionResolution", () => {
    it("keeps the initial query state distinct from memory-only", () => {
        h.connections = undefined
        h.isLoading = true
        const { result } = renderHook(() => (
            useDeviceConnectionResolution("conn-1")
        ))
        expect(result.current.status).toBe("loading")
        h.isLoading = false
    })

    it("prefers manager device identity and falls back to standalone client id", () => {
        h.connections = [{
            connection_id: "conn-1",
            device_id: "manager-1",
            version_info: { client_id: "client-1" },
        }]
        const manager = renderHook(() => (
            useDeviceConnectionResolution("conn-1")
        ))
        expect(manager.result.current).toMatchObject({
            status: "persistent",
            deviceKey: "device:manager-1",
        })

        h.connections = [{
            connection_id: "conn-1",
            version_info: { client_id: "client-1" },
        }]
        const standalone = renderHook(() => (
            useDeviceConnectionResolution("conn-1")
        ))
        expect(standalone.result.current).toMatchObject({
            status: "persistent",
            deviceKey: "client:client-1",
        })
    })

    it("uses memory-only after a completed lookup has no stable identity", () => {
        h.connections = [{ connection_id: "conn-1", version_info: {} }]
        h.isLoading = false
        const { result } = renderHook(() => (
            useDeviceConnectionResolution("conn-1")
        ))
        expect(result.current.status).toBe("memory-only")
    })
})
