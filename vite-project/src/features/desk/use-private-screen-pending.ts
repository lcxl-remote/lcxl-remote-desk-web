import { useCallback, useEffect, useRef, useState } from "react"

type Pending = { target: boolean; timer: number }

export function usePrivateScreenPending({
    onError,
    timeoutMs = 10_000,
}: {
    onError: (kind: "remote" | "timeout", message?: string) => void
    timeoutMs?: number
}) {
    const [target, setTarget] = useState<boolean | null>(null)
    const pendingRef = useRef<Pending | null>(null)
    const onErrorRef = useRef(onError)
    onErrorRef.current = onError

    const clear = useCallback(() => {
        const pending = pendingRef.current
        if (pending) window.clearTimeout(pending.timer)
        pendingRef.current = null
        setTarget(null)
    }, [])

    const start = useCallback((nextTarget: boolean) => {
        if (pendingRef.current) return false
        const timer = window.setTimeout(() => {
            if (pendingRef.current?.target !== nextTarget) return
            pendingRef.current = null
            setTarget(null)
            onErrorRef.current("timeout")
        }, timeoutMs)
        pendingRef.current = { target: nextTarget, timer }
        setTarget(nextTarget)
        return true
    }, [timeoutMs])

    const confirm = useCallback((visible: boolean) => {
        if (pendingRef.current?.target === visible) clear()
    }, [clear])

    const fail = useCallback((message?: string) => {
        if (!pendingRef.current) return
        clear()
        onErrorRef.current("remote", message)
    }, [clear])

    useEffect(() => () => {
        const pending = pendingRef.current
        if (pending) window.clearTimeout(pending.timer)
        pendingRef.current = null
    }, [])

    return {
        pending: target !== null,
        target,
        start,
        confirm,
        fail,
        clear,
    }
}
