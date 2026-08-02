import { fireEvent, render, waitFor } from "@testing-library/react"
import { beforeEach, describe, expect, it, vi } from "vitest"

vi.mock("react-i18next", () =>
    import("@/test-utils/i18n-mock").then((module) => module.reactI18nextMock()),
)

const toast = vi.hoisted(() => vi.fn())
vi.mock("@/hooks/use-toast", () => ({ useToast: () => ({ toast }) }))

import { ModelSelector } from "./model-selector"

function deferredResponse() {
    let resolve!: (value: Response) => void
    const promise = new Promise<Response>((resolvePromise) => {
        resolve = resolvePromise
    })
    return { promise, resolve }
}

function jsonResponse(data: unknown): Response {
    return {
        ok: true,
        json: async () => data,
    } as Response
}

describe("ModelSelector persistence queue", () => {
    beforeEach(() => {
        toast.mockReset()
    })

    it("serializes rapid choices and persists the final selection last", async () => {
        const firstPut = deferredResponse()
        const secondPut = deferredResponse()
        const putBodies: Array<{ model_id: number }> = []
        let putCount = 0
        global.fetch = vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
            const url = String(input)
            if (url === "/api/my/ai/models") {
                return Promise.resolve(jsonResponse({
                    success: true,
                    code: 0,
                    data: [
                        { model_id: 1, display_name: "One", role: "agent", tier: "personal", is_default: true, is_current_preference: true },
                        { model_id: 2, display_name: "Two", role: "agent", tier: "personal", is_default: false, is_current_preference: false },
                        { model_id: 3, display_name: "Three", role: "agent", tier: "personal", is_default: false, is_current_preference: false },
                    ],
                }))
            }
            if (url === "/api/billing/my/wallet") {
                return Promise.resolve(jsonResponse({ success: false, code: 1 }))
            }
            if (url === "/api/my/ai/model-preference" && init?.method === "PUT") {
                putBodies.push(JSON.parse(String(init.body)))
                putCount += 1
                return putCount === 1 ? firstPut.promise : secondPut.promise
            }
            return Promise.reject(new Error(`unexpected fetch: ${url}`))
        }) as typeof fetch
        const onChange = vi.fn()
        const { container } = render(<ModelSelector role="agent" onChange={onChange} />)
        const select = await waitFor(() => {
            const element = container.querySelector("select")
            expect(element).not.toBeNull()
            return element as HTMLSelectElement
        })

        fireEvent.change(select, { target: { value: "2" } })
        fireEvent.change(select, { target: { value: "3" } })

        expect(putBodies).toEqual([{ model_id: 2, role: "agent" }])
        firstPut.resolve(jsonResponse({ success: true, code: 0 }))
        await waitFor(() => expect(putBodies).toEqual([
            { model_id: 2, role: "agent" },
            { model_id: 3, role: "agent" },
        ]))

        secondPut.resolve(jsonResponse({ success: true, code: 0 }))
        await waitFor(() => expect(container).not.toHaveTextContent("Saving…"))
        expect(onChange).toHaveBeenLastCalledWith(3)
    })
})
