import { fireEvent, render, screen } from "@testing-library/react"
import { describe, expect, it, vi } from "vitest"
import { useFollowLatest } from "./use-follow-latest"

function FollowLatestHarness({ tick }: { tick: number }) {
    const { scrollRef, onScroll, showJumpToLatest, jumpToLatest } =
        useFollowLatest()
    return (
        <div>
            <div ref={scrollRef} onScroll={onScroll} data-testid="scroll-area">
                <span>{tick}</span>
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
