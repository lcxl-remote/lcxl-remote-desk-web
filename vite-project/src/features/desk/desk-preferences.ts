import type { AudioDataFlow, ConnectionModel } from "@/services/types"

export type WaylandControlMode = "auto" | "none" | "uinput" | "portal"

export interface DeskDevicePreferencesV1 {
    version: 1
    captureAudio: boolean
    imageCapture: string | null
    videoDeviceName: string | null
    showMouse: boolean
    adaptiveWebPageResolution: boolean
    videoEncoder: string | null
    videoQuality: number
    videoFps: number | null
    enableDirtyRect: boolean
    audioCapture: string | null
    audioDevice: {
        audioDataFlow: AudioDataFlow
        audioDeviceId: string
    } | null
    audioEncoder: string | null
    waylandControlMode: WaylandControlMode
}

export interface DeskUserPreferencesV1 {
    version: 1
    adaptiveQualityEnabled: boolean
    adaptiveBitrateEnabled: boolean
}

export interface PreferenceStorageAdapter {
    getItem(key: string): string | null
    setItem(key: string, value: string): void
}

export interface DeskPreferenceScope {
    controllerUserKey: string | null
    deviceKey: string | null
    restricted: boolean
}

const DEVICE_KEY_PREFIX = "lrdm.remoteDesk.devicePreferences.v1:"
const USER_KEY_PREFIX = "lrdm.remoteDesk.userPreferences.v1:"
const AUDIO_DATA_FLOWS = new Set<AudioDataFlow>(["Render", "Capture"])
const WAYLAND_CONTROL_MODES = new Set<WaylandControlMode>([
    "auto",
    "none",
    "uinput",
    "portal",
])

export const DEFAULT_DESK_DEVICE_PREFERENCES: DeskDevicePreferencesV1 = {
    version: 1,
    captureAudio: true,
    imageCapture: null,
    videoDeviceName: null,
    showMouse: true,
    adaptiveWebPageResolution: true,
    videoEncoder: null,
    videoQuality: 22,
    videoFps: null,
    enableDirtyRect: true,
    audioCapture: null,
    audioDevice: null,
    audioEncoder: null,
    waylandControlMode: "auto",
}

export const DEFAULT_DESK_USER_PREFERENCES: DeskUserPreferencesV1 = {
    version: 1,
    adaptiveQualityEnabled: true,
    adaptiveBitrateEnabled: true,
}

function cloneDevicePreferences(
    value: DeskDevicePreferencesV1,
): DeskDevicePreferencesV1 {
    return {
        ...value,
        audioDevice: value.audioDevice ? { ...value.audioDevice } : null,
    }
}

function cloneUserPreferences(
    value: DeskUserPreferencesV1,
): DeskUserPreferencesV1 {
    return { ...value }
}

function isNullableNonEmptyString(value: unknown): value is string | null {
    return value === null
        || (typeof value === "string" && value.trim().length > 0)
}

function isFiniteNumber(value: unknown): value is number {
    return typeof value === "number" && Number.isFinite(value)
}

export function parseDeskDevicePreferences(
    raw: string,
): DeskDevicePreferencesV1 | null {
    let value: unknown
    try {
        value = JSON.parse(raw)
    } catch {
        return null
    }
    if (!value || typeof value !== "object" || Array.isArray(value)) return null

    const candidate = value as Record<string, unknown>
    const expectedKeys = new Set([
        "version",
        "captureAudio",
        "imageCapture",
        "videoDeviceName",
        "showMouse",
        "adaptiveWebPageResolution",
        "videoEncoder",
        "videoQuality",
        "videoFps",
        "enableDirtyRect",
        "audioCapture",
        "audioDevice",
        "audioEncoder",
        "waylandControlMode",
    ])
    const audioDevice = candidate.audioDevice
    const validAudioDevice = audioDevice === null || (
        !!audioDevice
        && typeof audioDevice === "object"
        && !Array.isArray(audioDevice)
        && AUDIO_DATA_FLOWS.has(
            (audioDevice as Record<string, unknown>).audioDataFlow as AudioDataFlow,
        )
        && typeof (audioDevice as Record<string, unknown>).audioDeviceId === "string"
        && ((audioDevice as Record<string, unknown>).audioDeviceId as string).trim().length > 0
    )

    if (
        candidate.version !== 1
        || Object.keys(candidate).some((key) => !expectedKeys.has(key))
        || typeof candidate.captureAudio !== "boolean"
        || !isNullableNonEmptyString(candidate.imageCapture)
        || !isNullableNonEmptyString(candidate.videoDeviceName)
        || typeof candidate.showMouse !== "boolean"
        || typeof candidate.adaptiveWebPageResolution !== "boolean"
        || !isNullableNonEmptyString(candidate.videoEncoder)
        || !isFiniteNumber(candidate.videoQuality)
        || !Number.isInteger(candidate.videoQuality)
        || candidate.videoQuality < 0
        || candidate.videoQuality > 63
        || !(candidate.videoFps === null || (
            isFiniteNumber(candidate.videoFps)
            && Number.isInteger(candidate.videoFps)
            && candidate.videoFps > 0
        ))
        || typeof candidate.enableDirtyRect !== "boolean"
        || !isNullableNonEmptyString(candidate.audioCapture)
        || !validAudioDevice
        || !isNullableNonEmptyString(candidate.audioEncoder)
        || !WAYLAND_CONTROL_MODES.has(candidate.waylandControlMode as WaylandControlMode)
    ) {
        return null
    }

    return cloneDevicePreferences(candidate as unknown as DeskDevicePreferencesV1)
}

export function parseDeskUserPreferences(
    raw: string,
): DeskUserPreferencesV1 | null {
    let value: unknown
    try {
        value = JSON.parse(raw)
    } catch {
        return null
    }
    if (!value || typeof value !== "object" || Array.isArray(value)) return null
    const candidate = value as Record<string, unknown>
    if (
        candidate.version !== 1
        || Object.keys(candidate).some((key) => ![
            "version",
            "adaptiveQualityEnabled",
            "adaptiveBitrateEnabled",
        ].includes(key))
        || typeof candidate.adaptiveQualityEnabled !== "boolean"
        || typeof candidate.adaptiveBitrateEnabled !== "boolean"
    ) {
        return null
    }
    return cloneUserPreferences(candidate as unknown as DeskUserPreferencesV1)
}

export function devicePreferenceStorageKey(
    scope: DeskPreferenceScope,
): string | null {
    if (
        scope.restricted
        || !scope.controllerUserKey
        || !scope.deviceKey
    ) {
        return null
    }
    return `${DEVICE_KEY_PREFIX}${scope.controllerUserKey}:${scope.deviceKey}`
}

export function userPreferenceStorageKey(
    controllerUserKey: string | null,
): string | null {
    return controllerUserKey
        ? `${USER_KEY_PREFIX}${controllerUserKey}:global`
        : null
}

/**
 * Resolve the stable identity used only inside preference keys. The live
 * connection id is intentionally absent: manager devices use their persistent
 * device handle, while standalone hosts use their persistent client id.
 */
export function preferenceDeviceKey(
    connection: ConnectionModel | null | undefined,
): string | null {
    const managerDeviceId = connection?.device_id?.trim()
    if (managerDeviceId) return `device:${managerDeviceId}`
    const standaloneClientId = connection?.version_info?.client_id?.trim()
    return standaloneClientId ? `client:${standaloneClientId}` : null
}

/**
 * Session-local preference state with optional browser persistence. Memory is
 * always updated first, so quota errors, private mode, restricted grants, and
 * missing stable identities degrade to a working page-local configuration.
 */
export class DeskPreferenceStore {
    private readonly storage: PreferenceStorageAdapter | null
    private readonly deviceMemory = new Map<string, DeskDevicePreferencesV1>()
    private readonly userMemory = new Map<string, DeskUserPreferencesV1>()

    constructor(storage: PreferenceStorageAdapter | null) {
        this.storage = storage
    }

    /** Returns null until this browser scope has an actual saved/page-local choice. */
    loadDeviceIfPresent(
        scope: DeskPreferenceScope,
    ): DeskDevicePreferencesV1 | null {
        const key = devicePreferenceStorageKey(scope)
        const memoryKey = key
            ?? `memory:${scope.restricted ? "restricted" : "unresolved"}:${scope.deviceKey ?? "device"}`
        const memoryValue = this.deviceMemory.get(memoryKey) ?? null
        if (!key || !this.storage) {
            return memoryValue ? cloneDevicePreferences(memoryValue) : null
        }
        try {
            const raw = this.storage.getItem(key)
            const parsed = raw ? parseDeskDevicePreferences(raw) : null
            if (parsed) {
                this.deviceMemory.set(memoryKey, parsed)
                return cloneDevicePreferences(parsed)
            }
        } catch {
            // Keep the last page-local value when browser storage is unavailable.
        }
        return memoryValue ? cloneDevicePreferences(memoryValue) : null
    }

    loadDevice(scope: DeskPreferenceScope): DeskDevicePreferencesV1 {
        return this.loadDeviceIfPresent(scope)
            ?? cloneDevicePreferences(DEFAULT_DESK_DEVICE_PREFERENCES)
    }

    saveDevice(
        scope: DeskPreferenceScope,
        value: DeskDevicePreferencesV1,
    ): void {
        const key = devicePreferenceStorageKey(scope)
        const memoryKey = key
            ?? `memory:${scope.restricted ? "restricted" : "unresolved"}:${scope.deviceKey ?? "device"}`
        this.deviceMemory.set(memoryKey, cloneDevicePreferences(value))
        if (!key || !this.storage) return
        try {
            this.storage.setItem(key, JSON.stringify(value))
        } catch {
            // Runtime state remains authoritative for this page.
        }
    }

    loadUser(controllerUserKey: string | null): DeskUserPreferencesV1 {
        const key = userPreferenceStorageKey(controllerUserKey)
        const memoryKey = key ?? "memory:user"
        const memoryValue = this.userMemory.get(memoryKey)
            ?? DEFAULT_DESK_USER_PREFERENCES
        if (!key || !this.storage) return cloneUserPreferences(memoryValue)
        try {
            const raw = this.storage.getItem(key)
            const parsed = raw ? parseDeskUserPreferences(raw) : null
            if (parsed) {
                this.userMemory.set(memoryKey, parsed)
                return cloneUserPreferences(parsed)
            }
        } catch {
            // Keep the last page-local value when browser storage is unavailable.
        }
        return cloneUserPreferences(memoryValue)
    }

    saveUser(
        controllerUserKey: string | null,
        value: DeskUserPreferencesV1,
    ): void {
        const key = userPreferenceStorageKey(controllerUserKey)
        const memoryKey = key ?? "memory:user"
        this.userMemory.set(memoryKey, cloneUserPreferences(value))
        if (!key || !this.storage) return
        try {
            this.storage.setItem(key, JSON.stringify(value))
        } catch {
            // Runtime state remains authoritative for this page.
        }
    }
}
