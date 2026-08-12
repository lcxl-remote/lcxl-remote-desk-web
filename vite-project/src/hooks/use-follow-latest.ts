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
    const previousScrollHeightRef = useRef(0)
    const [showJumpToLatest, setShowJumpToLatest] = useState(false)

    const isAtBottom = useCallback((element: HTMLDivElement) => {
        return (
            element.scrollHeight - element.clientHeight - element.scrollTop <=
            BOTTOM_THRESHOLD_PX
        )
    }, [])

    const scrollToLatest = useCallback((element: HTMLDivElement) => {
        const target = Math.max(0, element.scrollHeight - element.clientHeight)
        // Avoid redundant writes. A focused textarea may make the browser adjust
        // its ancestor just enough to keep the caret visible; continuously
        // assigning scrollTop on unrelated renders fights that native behavior
        // and makes the content visibly oscillate.
        if (Math.abs(element.scrollTop - target) > 1) {
            element.scrollTop = target
        }
    }, [])

    const jumpToLatest = useCallback(() => {
        const element = scrollRef.current
        if (!element) return

        scrollToLatest(element)
        followingRef.current = true
        setShowJumpToLatest(false)
    }, [scrollToLatest])

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
        const contentHeightChanged =
            element.scrollHeight !== previousScrollHeightRef.current
        previousScrollHeightRef.current = element.scrollHeight

        if (!enabled) {
            previousEnabledRef.current = false
            followingRef.current = true
            setShowJumpToLatest(false)
            return
        }

        const becameEnabled = !previousEnabledRef.current
        if (becameEnabled) followingRef.current = true
        previousEnabledRef.current = true

        if (followingRef.current) {
            if (contentHeightChanged || becameEnabled) scrollToLatest(element)
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
                scrollToLatest(element)
                setShowJumpToLatest(false)
            } else {
                const atBottom = isAtBottom(element)
                if (atBottom) followingRef.current = true
                setShowJumpToLatest(!atBottom)
            }
        })
        observer.observe(element)
        return () => observer.disconnect()
    }, [enabled, isAtBottom, scrollToLatest])

    return {
        scrollRef,
        onScroll,
        showJumpToLatest,
        jumpToLatest,
    }
}
