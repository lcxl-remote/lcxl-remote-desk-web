import { describe, it, expect, vi } from "vitest"
import { renderHook } from "@testing-library/react"

// Mutable connection list backing the mocked query hook.
const h = vi.hoisted(() => ({
    connections: undefined as unknown[] | undefined,
    isLoading: false,
}))
vi.mock("@/services/hooks/connectionController/useListConnections", () => ({
    useListConnections: () => ({
        data: { success: true, code: 0, data: h.connections },
        isLoading: h.isLoading,
    }),
}))

import {
    useDeviceConnection,
    useDeviceConnectionResolution,
} from "./use-device-id"

describe("useDeviceConnection", () => {
    it("exposes the host-reported platform with the connection metadata", () => {
        h.connections = [{
            connection_id: "conn-1",
            device_id: "42",
            version_info: null, peer: { remote_desk_type:'server', operation_system:"Mac", display_name:null }, exec_capabilities: { available_exec_shells: [], exec_pty:false, exec_pty_elevation:false, max_ai_command_runtime_ms:null }, ai_assistant_settings: null,
        }]
        const { result } = renderHook(() => useDeviceConnection("conn-1"))
        expect(result.current?.peer.operation_system).toBe("Mac")
    })

    it("resolves the manager device id for a connection", () => {
        h.connections = [
            { connection_id: "conn-1", device_id: "42" },
            { connection_id: "conn-2", device_id: "7" },
        ]
        const { result } = renderHook(() => useDeviceConnection("conn-2"))
        expect(result.current?.device_id).toBe("7")
    })

    it("returns undefined for an OSS connection with no device id", () => {
        // The OSS single-instance signal leaves device_id unset; callers then
        // fall back to routing by connection_id (dual-target wire model).
        h.connections = [{ connection_id: "conn-1" }]
        const { result } = renderHook(() => useDeviceConnection("conn-1"))
        expect(result.current?.device_id).toBeUndefined()
    })

    it("returns undefined when the connection is unknown", () => {
        h.connections = [{ connection_id: "conn-1", device_id: "42" }]
        const { result } = renderHook(() => useDeviceConnection("missing"))
        expect(result.current?.device_id).toBeUndefined()
    })

    it("returns undefined when no connection id is given", () => {
        h.connections = [{ connection_id: "conn-1", device_id: "42" }]
        const { result } = renderHook(() => useDeviceConnection(undefined))
        expect(result.current?.device_id).toBeUndefined()
    })

    it("tolerates a not-yet-loaded connection list", () => {
        h.connections = undefined
        const { result } = renderHook(() => useDeviceConnection("conn-1"))
        expect(result.current?.device_id).toBeUndefined()
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

    it("uses the public device identity from either host", () => {
        h.connections = [{
            connection_id: "conn-1",
            device_id: "manager-1",
            version_info: null, peer: { remote_desk_type:'server', operation_system:'Other', display_name:null }, exec_capabilities: { available_exec_shells: [], exec_pty:false, exec_pty_elevation:false, max_ai_command_runtime_ms:null }, ai_assistant_settings: null,
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
            version_info: null, peer: { remote_desk_type:'server', operation_system:'Other', display_name:null }, exec_capabilities: { available_exec_shells: [], exec_pty:false, exec_pty_elevation:false, max_ai_command_runtime_ms:null }, ai_assistant_settings: null, device_id: "client-1",
        }]
        const standalone = renderHook(() => (
            useDeviceConnectionResolution("conn-1")
        ))
        expect(standalone.result.current).toMatchObject({
            status: "persistent",
            deviceKey: "device:client-1",
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
