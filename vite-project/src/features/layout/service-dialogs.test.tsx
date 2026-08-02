import { act, fireEvent, render, screen, waitFor } from "@testing-library/react"
import { beforeEach, describe, expect, it, vi } from "vitest"

vi.mock("react-i18next", () =>
    import("@/test-utils/i18n-mock").then((module) => module.reactI18nextMock()),
)

const toast = vi.hoisted(() => vi.fn())
vi.mock("@/hooks/use-toast", () => ({ useToast: () => ({ toast }) }))

import { ServiceInstallDialog } from "./service-install-dialog"
import { ServiceUninstallDialog } from "./service-uninstall-dialog"

function deferredResponse() {
    let resolve!: (value: Response) => void
    const promise = new Promise<Response>((resolvePromise) => {
        resolve = resolvePromise
    })
    return { promise, resolve }
}

function response(body: unknown): Response {
    return {
        ok: true,
        json: async () => body,
    } as Response
}

describe("service mutation dialogs", () => {
    beforeEach(() => {
        toast.mockReset()
    })

    it("keeps install open and pending until the request settles", async () => {
        const install = deferredResponse()
        const fetchMock = vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
            if (String(input).endsWith("/driver/status")) {
                return Promise.resolve(response({
                    code: 0,
                    data: { files_available: true, can_modify: true },
                }))
            }
            if (String(input) === "/api/service/install" && init?.method === "POST") {
                return install.promise
            }
            return Promise.reject(new Error("unexpected request"))
        })
        global.fetch = fetchMock as typeof fetch
        const onOpenChange = vi.fn()
        render(
            <ServiceInstallDialog
                open
                onOpenChange={onOpenChange}
                defaultInstallPath="C:\\Program Files\\lcxl"
            />,
        )
        const confirm = await screen.findByRole("button", { name: "Install Service" })

        fireEvent.click(confirm)
        fireEvent.click(confirm)

        expect(fetchMock.mock.calls.filter(([url]) => url === "/api/service/install")).toHaveLength(1)
        expect(confirm).toBeDisabled()
        expect(confirm).toHaveTextContent("Installing…")
        expect(onOpenChange).not.toHaveBeenCalledWith(false)

        act(() => install.resolve(response({ code: 0 })))
        await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false))
    })

    it("keeps uninstall pending and rejects duplicate confirmation", async () => {
        const uninstall = deferredResponse()
        global.fetch = vi.fn(() => uninstall.promise) as typeof fetch
        const onOpenChange = vi.fn()
        render(<ServiceUninstallDialog open onOpenChange={onOpenChange} />)
        const confirm = screen.getByRole("button", { name: /uninstall/i })

        fireEvent.click(confirm)
        fireEvent.click(confirm)

        expect(global.fetch).toHaveBeenCalledTimes(1)
        expect(confirm).toBeDisabled()
        expect(confirm).toHaveTextContent("Uninstalling…")

        act(() => uninstall.resolve(response({ code: 0 })))
        await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false))
    })
})
