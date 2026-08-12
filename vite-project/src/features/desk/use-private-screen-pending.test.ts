import { act, renderHook } from "@testing-library/react"
import { afterEach, describe, expect, it, vi } from "vitest"

import { usePrivateScreenPending } from "./use-private-screen-pending"

afterEach(() => {
    vi.useRealTimers()
})

describe("usePrivateScreenPending", () => {
    it("settles only when the status push confirms the requested target", () => {
        const onError = vi.fn()
        const { result } = renderHook(() => usePrivateScreenPending({ onError }))
        act(() => expect(result.current.start(true, "request-1")).toBe(true))
        expect(result.current.pending).toBe(true)

        act(() => result.current.confirm("request-1", false))
        expect(result.current.pending).toBe(true)
        act(() => result.current.confirm("request-1", true))
        expect(result.current.pending).toBe(false)
        expect(onError).not.toHaveBeenCalled()
    })

    it("surfaces an explicit remote error and permits retry", () => {
        const onError = vi.fn()
        const { result } = renderHook(() => usePrivateScreenPending({ onError }))
        act(() => result.current.start(true, "request-1"))
        act(() => result.current.fail("request-1", "driver unavailable"))

        expect(result.current.pending).toBe(false)
        expect(onError).toHaveBeenCalledWith("remote", "driver unavailable")
        act(() => expect(result.current.start(true, "request-2")).toBe(true))
    })

    it("times out and cancels its watchdog on unmount", () => {
        vi.useFakeTimers()
        const onError = vi.fn()
        const { result, unmount } = renderHook(() =>
            usePrivateScreenPending({ onError, timeoutMs: 100 }),
        )
        act(() => result.current.start(true, "request-1"))
        act(() => vi.advanceTimersByTime(100))
        expect(result.current.pending).toBe(false)
        expect(onError).toHaveBeenCalledWith("timeout")

        act(() => result.current.start(false, "request-2"))
        unmount()
        act(() => vi.runAllTimers())
        expect(onError).toHaveBeenCalledTimes(1)
    })
})
