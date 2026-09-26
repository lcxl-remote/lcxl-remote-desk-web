import { useEffect, useRef, useState } from 'react'
import { deskErrorCodeEnum } from '@/services/types'
import type { ServiceOperationStatus } from '@/services/types'

export const SERVICE_OPERATION_EVENT = 'lrd-service-operation'
const WAIT_MS = 600_000

export class ServiceRequestError extends Error {
    readonly code: number | undefined
    readonly busy: boolean

    constructor(code: number | undefined, message: string | undefined, busy = false) {
        super(message)
        this.code = code
        this.busy = busy
    }
}

function receipt(value: unknown): value is ServiceOperationStatus {
    if (!value || typeof value !== 'object') return false
    const item = value as ServiceOperationStatus
    return typeof item.operation_id === 'string' && item.operation_id.length > 0
        && ['install', 'uninstall'].includes(item.op)
        && ['queued', 'running', 'succeeded', 'cancelled', 'failed', 'unknown', 'submitted'].includes(item.state)
        && (item.error == null || ['authorization_not_granted', 'missing_pkexec', 'launch_failed',
            'installer_failed', 'busy', 'unsupported', 'connection_lost', 'timed_out'].includes(item.error))
        && (item.exit_code == null || Number.isInteger(item.exit_code))
}

/** Register native feedback before POST: completion can precede its HTTP reply. */
export function performServiceOperation(
    op: 'install' | 'uninstall',
    body: unknown,
    signal: AbortSignal,
    progress: (disconnected: boolean) => void,
): Promise<ServiceOperationStatus | undefined> {
    return new Promise((resolve, reject) => {
        const controller = new AbortController()
        const early = new Map<string, ServiceOperationStatus>()
        let id: string | undefined
        let ended = false
        let pollTimer: ReturnType<typeof setTimeout> | undefined
        let deadline: ReturnType<typeof setTimeout> | undefined
        const cleanup = () => {
            ended = true
            controller.abort()
            clearTimeout(pollTimer)
            clearTimeout(deadline)
            window.removeEventListener(SERVICE_OPERATION_EVENT, onNative)
            signal.removeEventListener('abort', onAbort)
            early.clear()
        }
        const finish = (result: ServiceOperationStatus | undefined) => {
            if (ended) return
            cleanup()
            resolve(result)
        }
        const unknown = (error: 'timed_out' | 'connection_lost') => finish({ operation_id: id ?? '', op, state: 'unknown', error, exit_code: null })
        const onAbort = () => {
            if (ended) return
            cleanup()
            reject(new DOMException('Request cancelled', 'AbortError'))
        }
        const onNative = (event: Event) => {
            const result: unknown = (event as CustomEvent).detail
            if (!receipt(result) || result.op !== op || ['queued', 'running'].includes(result.state)) return
            if (id === result.operation_id) finish(result)
            else if (!id) {
                if (early.size >= 8) early.delete(early.keys().next().value!)
                early.set(result.operation_id, result)
            }
        }
        const poll = async () => {
            if (ended || !id) return
            try {
                const response = await fetch(`/api/service/operations/${encodeURIComponent(id)}`, { signal: controller.signal })
                const data = await response.json().catch(() => null)
                if (ended) return
                const result = data?.data
                if (response.ok && data?.code === deskErrorCodeEnum.SUCCESS && receipt(result)
                    && result.operation_id === id && result.op === op) {
                    if (!['queued', 'running', 'unknown'].includes(result.state)) return finish(result)
                    // Daemon shutdown can precede the native completion event.
                    progress(result.state === 'unknown')
                } else progress(true)
            } catch {
                if (!ended) progress(true)
            }
            if (!ended) pollTimer = setTimeout(poll, 1000)
        }
        window.addEventListener(SERVICE_OPERATION_EVENT, onNative)
        signal.addEventListener('abort', onAbort, { once: true })
        deadline = setTimeout(() => unknown('timed_out'), WAIT_MS)
        if (signal.aborted) { onAbort(); return }
        void (async () => {
            try {
                const response = await fetch(`/api/service/${op}`, {
                    method: 'POST', signal: controller.signal,
                    headers: { 'Content-Type': 'application/json' },
                    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
                })
                const data = await response.json().catch(() => null)
                if (ended) return
                if (!data || typeof data.code !== 'number') { unknown('connection_lost'); return }
                if (data.code !== deskErrorCodeEnum.SUCCESS) {
                    cleanup()
                    reject(new ServiceRequestError(data?.code, data?.message, response.status === 409))
                    return
                }
                if (response.ok && data.data == null) {
                    // Older servers only acknowledge submission.
                    finish(undefined)
                    return
                }
                if (!response.ok || !receipt(data.data) || data.data.op !== op) {
                    unknown('connection_lost')
                    return
                }
                id = data.data.operation_id
                const cached = early.get(id!)
                early.clear()
                if (cached) finish(cached)
                else if (!['queued', 'running', 'unknown'].includes(data.data.state)) finish(data.data)
                else void poll()
            } catch {
                // A lost POST response does not prove that installation never began.
                if (!ended) unknown('connection_lost')
            }
        })()
    })
}

export function useServiceOperation() {
    const abort = useRef<AbortController | null>(null)
    const [disconnected, setDisconnected] = useState(false)
    useEffect(() => () => abort.current?.abort(), [])
    const run = (op: 'install' | 'uninstall', body?: unknown) => {
        abort.current?.abort()
        const controller = new AbortController()
        abort.current = controller
        setDisconnected(false)
        return performServiceOperation(op, body, controller.signal, setDisconnected)
    }
    return { run, disconnected }
}
