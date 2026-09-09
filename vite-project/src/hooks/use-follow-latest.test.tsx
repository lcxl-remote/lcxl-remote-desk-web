import { act, fireEvent, render, screen } from "@testing-library/react"
import { describe, expect, it, vi } from "vitest"
import { useFollowLatest } from "./use-follow-latest"

function FollowLatestHarness({ tick, conversation = "one" }: { tick: number; conversation?: string }) {
    const { scrollRef, contentRef, onScroll, showJumpToLatest, jumpToLatest } =
        useFollowLatest(true, conversation)
    return (
        <div>
            <div ref={scrollRef} onScroll={onScroll} data-testid="scroll-area">
                <div ref={contentRef}><span>{tick}</span></div>
                <textarea aria-label="Follow-up question" />
            </div>
            {showJumpToLatest && (
                <button type="button" onClick={jumpToLatest}>
                    Latest
                </button>
            )}
        </div>
    )
}

describe("useFollowLatest", () => {
    it("does not rewrite scrollTop on unrelated renders while a textarea is focused", () => {
        let scrollHeight = 1000
        let scrollTop = 700
        const writeScrollTop = vi.fn((value: number) => {
            scrollTop = value
        })
        const { rerender } = render(<FollowLatestHarness tick={0} />)
        const scrollArea = screen.getByTestId("scroll-area")
        Object.defineProperties(scrollArea, {
            scrollHeight: {
                configurable: true,
                get: () => scrollHeight,
            },
            clientHeight: { configurable: true, value: 300 },
            scrollTop: {
                configurable: true,
                get: () => scrollTop,
                set: writeScrollTop,
            },
        })

        // Establish the measured content height, then ignore that initial sync.
        rerender(<FollowLatestHarness tick={1} />)
        writeScrollTop.mockClear()

        screen.getByRole("textbox", { name: "Follow-up question" }).focus()
        rerender(<FollowLatestHarness tick={2} />)
        rerender(<FollowLatestHarness tick={3} />)
        expect(writeScrollTop).not.toHaveBeenCalled()

        // Actual appended content still follows exactly once to the new maximum.
        scrollHeight = 1100
        rerender(<FollowLatestHarness tick={4} />)
        expect(writeScrollTop).toHaveBeenCalledTimes(1)
        expect(writeScrollTop).toHaveBeenCalledWith(800)
    })

    it("jumps to the exact maximum scroll position", () => {
        const { rerender } = render(<FollowLatestHarness tick={0} />)
        const scrollArea = screen.getByTestId("scroll-area")
        Object.defineProperties(scrollArea, {
            scrollHeight: { configurable: true, value: 900 },
            clientHeight: { configurable: true, value: 300 },
        })
        scrollArea.scrollTop = 100
        fireEvent.scroll(scrollArea)
        rerender(<FollowLatestHarness tick={1} />)

        fireEvent.click(screen.getByRole("button", { name: "Latest" }))
        expect(scrollArea.scrollTop).toBe(600)
    })
})


describe("assistant conversation following", () => {
    it("preserves a reader's position during streaming, resumes on jump, and resets on conversation switch", () => {
        const { rerender } = render(<FollowLatestHarness tick={0} />)
        const area = screen.getByTestId("scroll-area")
        let height = 1000
        Object.defineProperties(area, {
            scrollHeight: { configurable: true, get: () => height },
            clientHeight: { configurable: true, value: 300 },
        })
        rerender(<FollowLatestHarness tick={1} />)
        expect(area.scrollTop).toBe(700)
        area.scrollTop = 200
        fireEvent.scroll(area)
        height = 1200
        rerender(<FollowLatestHarness tick={2} />)
        expect(area.scrollTop).toBe(200)
        fireEvent.click(screen.getByRole("button", { name: "Latest" }))
        expect(area.scrollTop).toBe(900)
        expect(screen.queryByRole("button", { name: "Latest" })).toBeNull()
        height = 1300
        rerender(<FollowLatestHarness tick={3} />)
        expect(area.scrollTop).toBe(1000)
        area.scrollTop = 200
        fireEvent.scroll(area)
        rerender(<FollowLatestHarness tick={4} conversation="two" />)
        expect(area.scrollTop).toBe(1000)
        expect(screen.queryByRole("button", { name: "Latest" })).toBeNull()
    })

    it("follows delayed image layout changes and resumes when manually scrolled to the bottom", () => {
        let resize = () => {}
        const observe = vi.fn()
        const disconnect = vi.fn()
        vi.stubGlobal("ResizeObserver", class {
            constructor(callback: () => void) { resize = callback }
            observe = observe
            disconnect = disconnect
        })
        try {
            const { unmount } = render(<FollowLatestHarness tick={0} />)
            const area = screen.getByTestId("scroll-area")
            let height = 900
            Object.defineProperties(area, {
                scrollHeight: { configurable: true, get: () => height },
                clientHeight: { configurable: true, value: 300 },
            })
            expect(observe).toHaveBeenCalledTimes(2)
            act(() => resize())
            expect(area.scrollTop).toBe(600)
            area.scrollTop = 100
            fireEvent.scroll(area)
            height = 1100
            act(() => resize())
            expect(area.scrollTop).toBe(100)
            area.scrollTop = 800
            fireEvent.scroll(area)
            expect(screen.queryByRole("button", { name: "Latest" })).toBeNull()
            height = 1200
            act(() => resize())
            expect(area.scrollTop).toBe(900)
            unmount()
            expect(disconnect).toHaveBeenCalled()
        } finally {
            vi.unstubAllGlobals()
        }
    })
})
