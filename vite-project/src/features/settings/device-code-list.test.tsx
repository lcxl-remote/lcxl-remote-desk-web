import { act, fireEvent, render, screen, waitFor } from "@testing-library/react"
import { beforeEach, describe, expect, it, vi } from "vitest"

vi.mock("react-i18next", () =>
    import("@/test-utils/i18n-mock").then((module) => module.reactI18nextMock()),
)
vi.mock("@/features/settings/capability-ceiling-editor", () => ({
    CapabilityCeilingEditor: () => <div />,
}))

const h = vi.hoisted(() => ({
    update: vi.fn(),
    refetch: vi.fn(),
}))
vi.mock("@/services/hooks/deviceCodeController/useListDeviceCodes", () => ({
    useListDeviceCodes: () => ({
        data: {
            data: {
                items: [{
                    id: 7,
                    clientId: "client-7",
                    deviceCode: "ABC234",
                    capabilities: null,
                    isOnline: false,
                    createdAt: "2026-01-01T00:00:00Z",
                    updatedAt: "2026-01-01T00:00:00Z",
                }],
            },
        },
        isLoading: false,
        isFetching: false,
        refetch: h.refetch,
    }),
}))
vi.mock("@/services/hooks/deviceCodeController/useCreateDeviceCode", () => ({
    useCreateDeviceCode: () => ({ mutateAsync: vi.fn(), isPending: false }),
}))
vi.mock("@/services/hooks/deviceCodeController/useUpdateDeviceCode", () => ({
    useUpdateDeviceCode: () => ({ mutateAsync: h.update }),
}))
vi.mock("@/services/hooks/deviceCodeController/useDeleteDeviceCode", () => ({
    useDeleteDeviceCode: () => ({ mutateAsync: vi.fn() }),
}))
vi.mock("@/services/hooks/deviceCodeController/useBatchDeleteDeviceCodes", () => ({
    useBatchDeleteDeviceCodes: () => ({ mutateAsync: vi.fn(), isPending: false }),
}))
vi.mock("@/hooks/use-toast", () => ({
    useToast: () => ({ toast: vi.fn() }),
}))

import { DeviceCodeList } from "./device-code-list"

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((resolvePromise) => {
        resolve = resolvePromise
    })
    return { promise, resolve }
}

describe("DeviceCodeList action ownership", () => {
    beforeEach(() => {
        h.update.mockReset()
        h.refetch.mockReset()
    })

    it("keeps edit pending through the required list refetch", async () => {
        const update = deferred<void>()
        const refetch = deferred<void>()
        h.update.mockReturnValue(update.promise)
        h.refetch.mockReturnValue(refetch.promise)
        render(<DeviceCodeList />)
        fireEvent.click(screen.getByTitle("Edit"))
        const save = await screen.findByRole("button", { name: "Save" })

        fireEvent.click(save)
        fireEvent.click(save)

        expect(h.update).toHaveBeenCalledTimes(1)
        expect(save).toBeDisabled()
        expect(save).toHaveTextContent("Saving…")

        act(() => update.resolve())
        await waitFor(() => expect(h.refetch).toHaveBeenCalledTimes(1))
        expect(save).toBeDisabled()
        expect(save).toHaveTextContent("Saving…")

        act(() => refetch.resolve())
        await waitFor(() => expect(screen.queryByRole("button", { name: "Saving…" })).toBeNull())
    })
})
