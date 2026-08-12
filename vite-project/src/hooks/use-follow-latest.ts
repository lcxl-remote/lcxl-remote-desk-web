import {
    type UIEventHandler,
    useCallback,
    useEffect,
    useLayoutEffect,
    useRef,
    useState,
} from "react"

const BOTTOM_THRESHOLD_PX = 24

/**
 * Chat-style scrolling: follow appended content while the reader is at the
 * bottom, but preserve their position as soon as they scroll up.
 */
export function useFollowLatest(enabled = true) {
    const scrollRef = useRef<HTMLDivElement>(null)
    const followingRef = useRef(true)
    const previousEnabledRef = useRef(enabled)
    const [showJumpToLatest, setShowJumpToLatest] = useState(false)

    const isAtBottom = useCallback((element: HTMLDivElement) => {
        return (
            element.scrollHeight - element.clientHeight - element.scrollTop <=
            BOTTOM_THRESHOLD_PX
        )
    }, [])

    const jumpToLatest = useCallback(() => {
        const element = scrollRef.current
        if (!element) return

        // Assigning scrollTop is deliberately immediate. Besides making the
        // action feel responsive, it avoids intermediate smooth-scroll events
        // being mistaken for the user scrolling away from the bottom.
        element.scrollTop = element.scrollHeight
        followingRef.current = true
        setShowJumpToLatest(false)
    }, [])

    const onScroll = useCallback<UIEventHandler<HTMLDivElement>>(
        (event) => {
            if (!enabled) return
            const atBottom = isAtBottom(event.currentTarget)
            followingRef.current = atBottom
            setShowJumpToLatest(!atBottom)
        },
        [enabled, isAtBottom],
    )

    // Run after every committed render because streaming text can grow without
    // changing a convenient scalar dependency. No state update occurs while the
    // visible jump-button state is already correct.
    useLayoutEffect(() => {
        const element = scrollRef.current
        if (!element) return

        if (!enabled) {
            previousEnabledRef.current = false
            followingRef.current = true
            setShowJumpToLatest(false)
            return
        }

        if (!previousEnabledRef.current) followingRef.current = true
        previousEnabledRef.current = true

        if (followingRef.current) {
            element.scrollTop = element.scrollHeight
            setShowJumpToLatest(false)
        } else {
            const atBottom = isAtBottom(element)
            // Content can shrink (reset/history switch) without a scroll event.
            // Treat that as reaching the bottom so subsequent output follows.
            if (atBottom) followingRef.current = true
            setShowJumpToLatest(!atBottom)
        }
    })

    // Native panel resizing changes the viewport without necessarily emitting a
    // scroll event. Keep a following reader pinned, or refresh the jump button
    // when a reader who is inspecting history resizes the panel.
    useEffect(() => {
        const element = scrollRef.current
        if (!element || !enabled || typeof ResizeObserver === "undefined") return

        const observer = new ResizeObserver(() => {
            if (followingRef.current) {
                element.scrollTop = element.scrollHeight
                setShowJumpToLatest(false)
            } else {
                const atBottom = isAtBottom(element)
                if (atBottom) followingRef.current = true
                setShowJumpToLatest(!atBottom)
            }
        })
        observer.observe(element)
        return () => observer.disconnect()
    }, [enabled, isAtBottom])

    return {
        scrollRef,
        onScroll,
        showJumpToLatest,
        jumpToLatest,
    }
}
