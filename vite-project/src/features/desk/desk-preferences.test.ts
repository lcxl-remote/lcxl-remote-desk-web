import { describe, expect, it } from "vitest"
import {
    DEFAULT_DESK_DEVICE_PREFERENCES,
    DeskPreferenceStore,
    devicePreferenceStorageKey,
    parseDeskDevicePreferences,
    parseDeskUserPreferences,
    preferenceDeviceKey,
    userPreferenceStorageKey,
} from "./desk-preferences"

class MemoryStorage {
    readonly values = new Map<string, string>()
    failReads = false
    failWrites = false

    getItem(key: string): string | null {
        if (this.failReads) throw new Error("storage disabled")
        return this.values.get(key) ?? null
    }

    setItem(key: string, value: string): void {
        if (this.failWrites) throw new Error("quota exceeded")
        this.values.set(key, value)
    }
}

const persistentScope = {
    controllerUserKey: "u:7",
    deviceKey: "device:abc",
    restricted: false,
}

describe("desk preference schema", () => {
    it("defaults system audio capture to enabled", () => {
        expect(DEFAULT_DESK_DEVICE_PREFERENCES.captureAudio).toBe(true)
    })

    it("rejects malformed JSON, unknown versions, and missing fields", () => {
        expect(parseDeskDevicePreferences("not json")).toBeNull()
        expect(parseDeskDevicePreferences(JSON.stringify({
            ...DEFAULT_DESK_DEVICE_PREFERENCES,
            version: 2,
        }))).toBeNull()
        const { videoQuality: _removed, ...missing } = DEFAULT_DESK_DEVICE_PREFERENCES
        expect(parseDeskDevicePreferences(JSON.stringify(missing))).toBeNull()
        expect(parseDeskUserPreferences(JSON.stringify({ version: 1 }))).toBeNull()
        expect(parseDeskDevicePreferences(JSON.stringify({
            ...DEFAULT_DESK_DEVICE_PREFERENCES,
            imageCapture: "",
        }))).toBeNull()
        expect(parseDeskDevicePreferences(JSON.stringify({
            ...DEFAULT_DESK_DEVICE_PREFERENCES,
            videoFps: -1,
        }))).toBeNull()
    })

    it("allows only null or an explicit non-empty physical audio device", () => {
        expect(parseDeskDevicePreferences(JSON.stringify({
            ...DEFAULT_DESK_DEVICE_PREFERENCES,
            audioDevice: null,
        }))).not.toBeNull()
        expect(parseDeskDevicePreferences(JSON.stringify({
            ...DEFAULT_DESK_DEVICE_PREFERENCES,
            audioDevice: { audioDataFlow: "Render", audioDeviceId: "speaker-1" },
        }))).not.toBeNull()
        expect(parseDeskDevicePreferences(JSON.stringify({
            ...DEFAULT_DESK_DEVICE_PREFERENCES,
            audioDevice: { audioDataFlow: "Render", audioDeviceId: "" },
        }))).toBeNull()
    })
})

describe("desk preference keys", () => {
    it("uses manager device id or standalone client id, never connection id", () => {
        expect(preferenceDeviceKey({
            connection_id: "volatile-1",
            device_id: "manager-device",
            version_info: { client_id: "standalone-client" } as never,
        })).toBe("device:manager-device")
        expect(preferenceDeviceKey({
            connection_id: "volatile-2",
            version_info: { client_id: "standalone-client" } as never,
        })).toBe("client:standalone-client")
        expect(preferenceDeviceKey({
            connection_id: "volatile-only",
            version_info: {} as never,
        })).toBeNull()
    })

    it("does not build persistent keys for grants or missing identities", () => {
        expect(devicePreferenceStorageKey(persistentScope)).toBe(
            "lrdm.remoteDesk.devicePreferences.v1:u:7:device:abc",
        )
        expect(devicePreferenceStorageKey({
            ...persistentScope,
            restricted: true,
        })).toBeNull()
        expect(devicePreferenceStorageKey({
            ...persistentScope,
            deviceKey: null,
        })).toBeNull()
        expect(userPreferenceStorageKey(null)).toBeNull()
    })
})

describe("DeskPreferenceStore", () => {
    it("distinguishes an absent preference from schema defaults", () => {
        const store = new DeskPreferenceStore(new MemoryStorage())
        expect(store.loadDeviceIfPresent(persistentScope)).toBeNull()
        expect(store.loadDevice(persistentScope)).toEqual(
            DEFAULT_DESK_DEVICE_PREFERENCES,
        )
    })

    it("persists and reloads valid device and user preferences", () => {
        const storage = new MemoryStorage()
        const writer = new DeskPreferenceStore(storage)
        writer.saveDevice(persistentScope, {
            ...DEFAULT_DESK_DEVICE_PREFERENCES,
            captureAudio: false,
            videoQuality: 18,
        })
        writer.saveUser("u:7", {
            version: 1,
            adaptiveQualityEnabled: false,
            adaptiveBitrateEnabled: true,
        })

        const reader = new DeskPreferenceStore(storage)
        expect(reader.loadDevice(persistentScope)).toMatchObject({
            captureAudio: false,
            videoQuality: 18,
        })
        expect(reader.loadUser("u:7")).toEqual({
            version: 1,
            adaptiveQualityEnabled: false,
            adaptiveBitrateEnabled: true,
        })
    })

    it("ignores corrupt persisted values and uses defaults", () => {
        const storage = new MemoryStorage()
        storage.values.set(devicePreferenceStorageKey(persistentScope)!, "{}")
        const store = new DeskPreferenceStore(storage)
        expect(store.loadDevice(persistentScope)).toEqual(
            DEFAULT_DESK_DEVICE_PREFERENCES,
        )
    })

    it("keeps page-local state when writes or reads fail", () => {
        const storage = new MemoryStorage()
        const store = new DeskPreferenceStore(storage)
        storage.failWrites = true
        store.saveDevice(persistentScope, {
            ...DEFAULT_DESK_DEVICE_PREFERENCES,
            videoQuality: 17,
        })
        storage.failReads = true
        expect(store.loadDevice(persistentScope).videoQuality).toBe(17)
    })

    it("does not leak page-local values between stable device scopes", () => {
        const store = new DeskPreferenceStore(null)
        store.saveDevice(persistentScope, {
            ...DEFAULT_DESK_DEVICE_PREFERENCES,
            videoQuality: 17,
        })
        expect(store.loadDevice({
            ...persistentScope,
            deviceKey: "device:other",
        }).videoQuality).toBe(22)
    })

    it("never reads or writes browser storage for restricted grants", () => {
        const storage = new MemoryStorage()
        storage.failReads = true
        storage.failWrites = true
        const store = new DeskPreferenceStore(storage)
        const restricted = { ...persistentScope, restricted: true }
        store.saveDevice(restricted, {
            ...DEFAULT_DESK_DEVICE_PREFERENCES,
            showMouse: false,
        })
        expect(store.loadDevice(restricted).showMouse).toBe(false)
        expect(storage.values.size).toBe(0)
    })
})
