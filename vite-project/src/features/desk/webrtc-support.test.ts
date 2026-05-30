import { afterEach, describe, expect, it } from "vitest"
import { isWebRtcAvailable } from "./webrtc-support"

describe("isWebRtcAvailable", () => {
    const original = globalThis.RTCPeerConnection

    afterEach(() => {
        globalThis.RTCPeerConnection = original
    })

    it("returns true when RTCPeerConnection exists", () => {
        // Minimal stub — only its presence is checked.
        globalThis.RTCPeerConnection = function () {} as unknown as typeof RTCPeerConnection
        expect(isWebRtcAvailable()).toBe(true)
    })

    it("returns false when RTCPeerConnection is undefined", () => {
        // @ts-expect-error simulate a webview without WebRTC
        delete globalThis.RTCPeerConnection
        expect(isWebRtcAvailable()).toBe(false)
    })
})
