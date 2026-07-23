import { useCallback, useEffect, useState } from "react"
import type {
    MouseEvent as ReactMouseEvent,
    RefObject,
} from "react"

type UseDraggableControlBarOptions = {
    controlBarRef: RefObject<HTMLDivElement | null>
    wrapperRef: RefObject<HTMLDivElement | null>
}

export function useDraggableControlBar({
    controlBarRef,
    wrapperRef,
}: UseDraggableControlBarOptions) {
    const [isDragging, setIsDragging] = useState(false)
    const [dragOffset, setDragOffset] = useState({ x: 0, y: 0 })

    const handleDragStart = (event: ReactMouseEvent) => {
        if (!controlBarRef.current || !wrapperRef.current) return

        setIsDragging(true)
        const controlBar = controlBarRef.current
        const wrapperRect = wrapperRef.current.getBoundingClientRect()
        const controlBarRect = controlBar.getBoundingClientRect()

        setDragOffset({
            x: event.clientX - controlBarRect.left,
            y: event.clientY - controlBarRect.top,
        })

        controlBar.style.transition = "none"
        controlBar.style.transform = "none"
        controlBar.style.bottom = "auto"
        controlBar.style.left = `${controlBarRect.left - wrapperRect.left}px`
        controlBar.style.top = `${controlBarRect.top - wrapperRect.top}px`
    }

    const handleDrag = useCallback((event: MouseEvent) => {
        if (!isDragging || !controlBarRef.current || !wrapperRef.current) return
        event.preventDefault()

        const controlBar = controlBarRef.current
        const wrapperRect = wrapperRef.current.getBoundingClientRect()
        const screenX = event.clientX - dragOffset.x
        const screenY = event.clientY - dragOffset.y
        const maxX = wrapperRect.width - controlBar.offsetWidth
        const maxY = wrapperRect.height - controlBar.offsetHeight
        const nextX = Math.max(0, Math.min(screenX - wrapperRect.left, maxX))
        const nextY = Math.max(0, Math.min(screenY - wrapperRect.top, maxY))

        controlBar.style.left = `${nextX}px`
        controlBar.style.top = `${nextY}px`
    }, [controlBarRef, dragOffset, isDragging, wrapperRef])

    const handleDragEnd = useCallback(() => {
        setIsDragging(false)
        if (controlBarRef.current) {
            controlBarRef.current.style.transition = ""
        }
    }, [controlBarRef])

    useEffect(() => {
        if (isDragging) {
            document.addEventListener("mousemove", handleDrag)
            document.addEventListener("mouseup", handleDragEnd)
        }

        return () => {
            document.removeEventListener("mousemove", handleDrag)
            document.removeEventListener("mouseup", handleDragEnd)
        }
    }, [handleDrag, handleDragEnd, isDragging])

    useEffect(() => {
        const wrapper = wrapperRef.current
        if (!wrapper) return

        const resizeObserver = new ResizeObserver(() => {
            const controlBar = controlBarRef.current
            if (!controlBar || controlBar.style.transform !== "none") return

            const wrapperRect = wrapper.getBoundingClientRect()
            const currentLeft = parseFloat(controlBar.style.left) || 0
            const currentTop = parseFloat(controlBar.style.top) || 0
            const maxX = wrapperRect.width - controlBar.offsetWidth
            const maxY = wrapperRect.height - controlBar.offsetHeight

            controlBar.style.left = `${Math.max(0, Math.min(currentLeft, maxX))}px`
            controlBar.style.top = `${Math.max(0, Math.min(currentTop, maxY))}px`
        })

        resizeObserver.observe(wrapper)
        return () => resizeObserver.disconnect()
    }, [controlBarRef, wrapperRef])

    return {
        handleDragStart,
        isDragging,
    }
}
