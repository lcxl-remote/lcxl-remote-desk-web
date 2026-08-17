import { act, fireEvent, render, screen, waitFor } from "@testing-library/react"
import { MemoryRouter, Route, Routes } from "react-router-dom"
import { beforeEach, describe, expect, it, vi } from "vitest"

vi.mock("react-i18next", () =>
    import("@/test-utils/i18n-mock").then((module) => module.reactI18nextMock()),
)

const h = vi.hoisted(() => ({
    listFiles: vi.fn(),
    toast: vi.fn(),
    querySystemInfo: vi.fn().mockResolvedValue(null),
    closeConnection: vi.fn(),
    prepareTransfers: vi.fn(),
}))
vi.mock("./use-file-transfer", async (importOriginal) => ({
    // The real module is kept for the error helpers the page imports alongside
    // the hook; only the hook itself is replaced.
    ...(await importOriginal<typeof import("./use-file-transfer")>()),
    useFileTransfer: () => ({
        transfers: [],
        downloadFile: vi.fn(),
        uploadFile: vi.fn(),
        cancelTransfer: vi.fn(),
        removeTransfer: vi.fn(),
        listFiles: h.listFiles,
        deleteFile: vi.fn(),
        querySystemInfo: h.querySystemInfo,
        closeConnection: h.closeConnection,
        prepareTransfers: h.prepareTransfers,
        channelStatus: 'ready' as const,
        channelFailure: null,
    }),
}))
vi.mock("@/features/desk/restricted-session", () => ({
    useRestrictedSession: () => ({ capabilityVisible: () => true }),
}))
vi.mock("@/hooks/use-toast", () => ({
    useToast: () => ({ toast: h.toast }),
}))

import FileList from "./file-list"

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((resolvePromise) => {
        resolve = resolvePromise
    })
    return { promise, resolve }
}

describe("FileList manual refresh", () => {
    beforeEach(() => h.listFiles.mockReset())

    it("keeps the refresh disabled and spinning until the listing settles", async () => {
        const initial = deferred<{ file_info_list: unknown[]; total_count: number }>()
        const refresh = deferred<{ file_info_list: unknown[]; total_count: number }>()
        h.listFiles
            .mockReturnValueOnce(initial.promise)
            .mockReturnValueOnce(refresh.promise)
        render(
            <MemoryRouter initialEntries={["/files/connection-1"]}>
                <Routes>
                    <Route path="/files/:id" element={<FileList />} />
                </Routes>
            </MemoryRouter>,
        )
        act(() => initial.resolve({ file_info_list: [], total_count: 0 }))
        const refreshButton = await screen.findByRole("button", { name: "Refresh" })
        await waitFor(() => expect(refreshButton).toBeEnabled())

        fireEvent.click(refreshButton)
        fireEvent.click(refreshButton)

        expect(h.listFiles).toHaveBeenCalledTimes(2)
        expect(refreshButton).toBeDisabled()
        expect(refreshButton).toHaveAttribute("aria-busy", "true")
        expect(refreshButton.querySelector("svg")).toHaveClass("animate-spin")

        act(() => refresh.resolve({ file_info_list: [], total_count: 0 }))
        await waitFor(() => expect(refreshButton).toBeEnabled())
        expect(refreshButton).not.toHaveAttribute("aria-busy")
    })
})
