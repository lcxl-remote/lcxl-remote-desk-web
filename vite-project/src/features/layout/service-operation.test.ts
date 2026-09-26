import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { performServiceOperation, SERVICE_OPERATION_EVENT, ServiceRequestError } from './service-operation'

function response(data: unknown, status = 200): Response {
    return { ok: status < 400, status, json: async () => data } as Response
}
function item(state = 'queued', operation_id = 'op-1', op = 'install') {
    return { operation_id, op, state, error: null, exit_code: null }
}
function native(data: unknown) {
    window.dispatchEvent(new CustomEvent(SERVICE_OPERATION_EVENT, { detail: data }))
}
async function flush() { await vi.advanceTimersByTimeAsync(0) }

describe('native service operation lifecycle', () => {
    beforeEach(() => vi.useFakeTimers())
    afterEach(() => { vi.useRealTimers(); vi.unstubAllGlobals() })

    it('waits after 202 and ignores unrelated receipts', async () => {
        const fetch = vi.fn().mockResolvedValue(response({ code: 0, data: item() }, 202))
        vi.stubGlobal('fetch', fetch)
        const settled = vi.fn()
        const pending = performServiceOperation('install', {}, new AbortController().signal, vi.fn()).then(settled)
        await flush()
        native(item('succeeded', 'other'))
        native(item('succeeded', 'op-1', 'uninstall'))
        expect(settled).not.toHaveBeenCalled()
        native(item('succeeded'))
        await pending
        expect(settled).toHaveBeenCalledWith(item('succeeded'))
        await vi.advanceTimersByTimeAsync(5000)
        expect(fetch).toHaveBeenCalledTimes(2)
    })

    it('keeps an early result until POST supplies its ID', async () => {
        let resolve!: (value: Response) => void
        vi.stubGlobal('fetch', vi.fn(() => new Promise<Response>(r => { resolve = r })))
        const pending = performServiceOperation('install', {}, new AbortController().signal, vi.fn())
        native(item('cancelled'))
        resolve(response({ code: 0, data: item() }, 202))
        expect((await pending)?.state).toBe('cancelled')
    })

    it('accepts the local completion after daemon shutdown without reposting', async () => {
        const progress = vi.fn()
        const fetch = vi.fn().mockResolvedValueOnce(response({ code: 0, data: item('queued', 'op-1', 'uninstall') }, 202))
            .mockRejectedValue(new TypeError('offline'))
        vi.stubGlobal('fetch', fetch)
        const pending = performServiceOperation('uninstall', undefined, new AbortController().signal, progress)
        await flush()
        await vi.advanceTimersByTimeAsync(3000)
        expect(progress).toHaveBeenCalledWith(true)
        native(item('succeeded', 'op-1', 'uninstall'))
        expect((await pending)?.state).toBe('succeeded')
        expect(fetch.mock.calls.filter(([url]) => url === '/api/service/uninstall')).toHaveLength(1)
    })

    it('returns unknown on timeout and removes listeners on abort', async () => {
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response({ code: 0, data: item() })))
        const pending = performServiceOperation('install', {}, new AbortController().signal, vi.fn())
        await vi.advanceTimersByTimeAsync(600_000)
        expect((await pending)?.state).toBe('unknown')
        const abort = new AbortController()
        const remove = vi.spyOn(window, 'removeEventListener')
        const stopped = performServiceOperation('install', {}, abort.signal, vi.fn())
        const checked = expect(stopped).rejects.toMatchObject({ name: 'AbortError' })
        abort.abort()
        await checked
        expect(remove).toHaveBeenCalledWith(SERVICE_OPERATION_EVENT, expect.any(Function))
        remove.mockRestore()
    })

    it('reports server rejection and an unconfirmed lost POST separately', async () => {
        vi.stubGlobal('fetch', vi.fn().mockResolvedValueOnce(response({ code: 8, message: 'busy' }, 409)))
        await expect(performServiceOperation('install', {}, new AbortController().signal, vi.fn()))
            .rejects.toMatchObject({ busy: true, code: 8 } satisfies Partial<ServiceRequestError>)
        vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new TypeError('lost response')))
        expect((await performServiceOperation('install', {}, new AbortController().signal, vi.fn()))?.state).toBe('unknown')
    })

    it('accepts an authenticated polled failure and does not wait for native feedback', async () => {
        const failed = { ...item('failed'), error: 'installer_failed', exit_code: 1 }
        vi.stubGlobal('fetch', vi.fn().mockResolvedValueOnce(response({ code: 0, data: item() }, 202))
            .mockResolvedValue(response({ code: 0, data: failed })))
        expect(await performServiceOperation('install', {}, new AbortController().signal, vi.fn())).toEqual(failed)
    })

    it('does not report malformed or mismatched submission receipts as accepted', async () => {
        for (const data of [ { code: 0, data: {} }, { code: 0, data: item('queued', 'op-1', 'uninstall') }, null ]) {
            vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response(data, 202)))
            expect((await performServiceOperation('install', {}, new AbortController().signal, vi.fn()))?.state).toBe('unknown')
        }
    })
})
