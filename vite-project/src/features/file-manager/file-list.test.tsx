import { act, fireEvent, render, screen, waitFor } from "@testing-library/react"
import { MemoryRouter, Route, Routes } from "react-router-dom"
import { beforeEach, describe, expect, it, vi } from "vitest"

vi.mock("react-i18next", () =>
    import("@/test-utils/i18n-mock").then((module) => module.reactI18nextMock()),
)

const h = vi.hoisted(() => ({
    listFiles: vi.fn(),
    uploadFile: vi.fn(),
    toast: vi.fn(),
    querySystemInfo: vi.fn().mockResolvedValue(null),
    closeConnection: vi.fn(),
    prepareTransfers: vi.fn(),
}))
vi.mock("@/services/hooks/connectionController/useListConnections", () => ({
    useListConnections: () => ({
        data: [{
            connection_id: "connection-1",
            version_info: { client_id: "stable-device-1" },
        }],
    }),
}))
vi.mock("./use-file-transfer", async (importOriginal) => ({
    // The real module is kept for the error helpers the page imports alongside
    // the hook; only the hook itself is replaced.
    ...(await importOriginal<typeof import("./use-file-transfer")>()),
    useFileTransfer: () => ({
        transfers: [],
        downloadFile: vi.fn(),
        uploadFile: h.uploadFile,
        cancelTransfer: vi.fn(),
        removeTransfer: vi.fn(),
        listFiles: h.listFiles,
        deleteFile: vi.fn(),
        querySystemInfo: h.querySystemInfo,
        closeConnection: h.closeConnection,
        prepareTransfers: h.prepareTransfers,
        channelStatus: 'ready' as const,
        channelFailure: null,
        sessionTargets: [],
        selectSessionTarget: vi.fn(),
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
        const initial = deferred<{ path: string; file_info_list: unknown[]; total_count: number }>()
        const refresh = deferred<{ path: string; file_info_list: unknown[]; total_count: number }>()
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
        act(() => initial.resolve({ path: "", file_info_list: [], total_count: 0 }))
        const refreshButton = await screen.findByRole("button", { name: "Refresh" })
        await waitFor(() => expect(refreshButton).toBeEnabled())
        expect(screen.queryByTitle("添加到 AI 助手")).toBeNull()

        fireEvent.click(refreshButton)
        fireEvent.click(refreshButton)

        expect(h.listFiles).toHaveBeenCalledTimes(2)
        expect(refreshButton).toBeDisabled()
        expect(refreshButton).toHaveAttribute("aria-busy", "true")
        expect(refreshButton.querySelector("svg")).toHaveClass("animate-spin")

        act(() => refresh.resolve({ path: "", file_info_list: [], total_count: 0 }))
        await waitFor(() => expect(refreshButton).toBeEnabled())
        expect(refreshButton).not.toHaveAttribute("aria-busy")
    })
})

describe("FileList initial user directory", () => {
    beforeEach(() => {
        h.listFiles.mockReset()
        h.uploadFile.mockReset()
    })

    function renderList() {
        return render(
            <MemoryRouter initialEntries={["/files/connection-1"]}>
                <Routes><Route path="/files/:id" element={<FileList />} /></Routes>
            </MemoryRouter>,
        )
    }

    it("uses the resolved home for refresh and upload without listing it twice", async () => {
        h.listFiles.mockResolvedValue({ path: "/home/user", file_info_list: [], total_count: 0 })
        const { container } = renderList()
        await screen.findByText("/home/user")
        expect(h.listFiles).toHaveBeenCalledExactlyOnceWith({
            path: "", prefer_user_home: true, page_no: 1, page_count: 100,
        })
        fireEvent.click(screen.getByRole("button", { name: "Refresh" }))
        await waitFor(() => expect(h.listFiles).toHaveBeenCalledTimes(2))
        expect(h.listFiles).toHaveBeenLastCalledWith({
            path: "/home/user", prefer_user_home: false, page_no: 1, page_count: 100,
        })
        const file = new File(["text"], "file.txt", { type: "text/plain" })
        fireEvent.change(container.querySelector('input[type="file"]')!, { target: { files: [file] } })
        expect(h.uploadFile).toHaveBeenCalledWith("/home/user", file)
    })

    it("preserves an explicit root visit after opening home", async () => {
        h.listFiles
            .mockResolvedValueOnce({ path: "/home/user", file_info_list: [], total_count: 0 })
            .mockResolvedValue({ path: "", file_info_list: [], total_count: 0 })
        renderList()
        await screen.findByText("/home/user")
        fireEvent.click(screen.getByRole("button", { name: "My Computer" }))
        await screen.findByText("My Computer")
        expect(h.listFiles).toHaveBeenLastCalledWith({
            path: "", prefer_user_home: false, page_no: 1, page_count: 100,
        })
    })

    it("keeps pagination in the resolved home directory", async () => {
        h.listFiles.mockResolvedValue({ path: "/home/user", file_info_list: [], total_count: 150 })
        renderList()
        await screen.findByText("/home/user")
        const pagination = screen.getByText("1 / 2").parentElement!
        fireEvent.click(pagination.querySelectorAll("button")[1])
        await waitFor(() => expect(h.listFiles).toHaveBeenCalledTimes(2))
        expect(h.listFiles).toHaveBeenLastCalledWith({
            path: "/home/user", prefer_user_home: false, page_no: 2, page_count: 100,
        })
    })

    it("re-resolves Home from root and on repeated Home clicks", async () => {
        h.listFiles
            .mockResolvedValueOnce({ path: "/home/user", file_info_list: [], total_count: 0 })
            .mockResolvedValueOnce({ path: "", file_info_list: [], total_count: 0 })
            .mockResolvedValueOnce({ path: "/home/changed", file_info_list: [], total_count: 0 })
            .mockResolvedValueOnce({ path: "/home/latest", file_info_list: [], total_count: 0 })
        renderList()
        await screen.findByText("/home/user")
        fireEvent.click(screen.getByRole("button", { name: "My Computer" }))
        await waitFor(() => expect(screen.getByRole("button", { name: "Refresh" })).toBeEnabled())
        fireEvent.click(screen.getByRole("button", { name: "User home" }))
        await screen.findByText("/home/changed")
        expect(h.listFiles).toHaveBeenLastCalledWith({
            path: "", prefer_user_home: true, page_no: 1, page_count: 100,
        })
        fireEvent.click(screen.getByRole("button", { name: "User home" }))
        await screen.findByText("/home/latest")
        expect(h.listFiles).toHaveBeenCalledTimes(4)
        expect(h.listFiles).toHaveBeenLastCalledWith({
            path: "", prefer_user_home: true, page_no: 1, page_count: 100,
        })
    })

    it("resets pagination and retries Home after the server previously fell back to root", async () => {
        h.listFiles.mockResolvedValue({ path: "", file_info_list: [], total_count: 150 })
        renderList()
        const pagination = (await screen.findByText("1 / 2")).parentElement!
        fireEvent.click(pagination.querySelectorAll("button")[1])
        await waitFor(() => expect(h.listFiles).toHaveBeenLastCalledWith({
            path: "", prefer_user_home: false, page_no: 2, page_count: 100,
        }))
        h.listFiles.mockResolvedValue({ path: "/home/user", file_info_list: [], total_count: 0 })
        fireEvent.click(screen.getByRole("button", { name: "User home" }))
        await screen.findByText("/home/user")
        expect(h.listFiles).toHaveBeenLastCalledWith({
            path: "", prefer_user_home: true, page_no: 1, page_count: 100,
        })
    })

    it("retries the home request when the initial request failed", async () => {
        h.listFiles.mockRejectedValueOnce(new Error("Connection interrupted"))
            .mockResolvedValue({ path: "/home/user", file_info_list: [], total_count: 0 })
        renderList()
        const refresh = screen.getByRole("button", { name: "Refresh" })
        await waitFor(() => expect(refresh).toBeEnabled())
        expect(screen.getByRole("button", { name: "Upload" })).toBeDisabled()
        fireEvent.click(refresh)
        await screen.findByText("/home/user")
        expect(h.listFiles).toHaveBeenLastCalledWith({
            path: "", prefer_user_home: true, page_no: 1, page_count: 100,
        })
    })

    it("keeps the root fallback on subsequent refreshes", async () => {
        h.listFiles.mockResolvedValue({ path: "", file_info_list: [], total_count: 0 })
        renderList()
        const refresh = await screen.findByRole("button", { name: "Refresh" })
        await waitFor(() => expect(refresh).toBeEnabled())
        fireEvent.click(refresh)
        await waitFor(() => expect(h.listFiles).toHaveBeenCalledTimes(2))
        expect(h.listFiles).toHaveBeenLastCalledWith({
            path: "", prefer_user_home: false, page_no: 1, page_count: 100,
        })
    })

    it("does not let a delayed initial listing override an explicit root visit", async () => {
        const initial = deferred<{ path: string; file_info_list: unknown[]; total_count: number }>()
        h.listFiles.mockReturnValueOnce(initial.promise)
            .mockResolvedValue({ path: "", file_info_list: [], total_count: 0 })
        renderList()
        expect(screen.getByRole("button", { name: "Upload" })).toBeDisabled()
        fireEvent.click(screen.getByRole("button", { name: "My Computer" }))
        await waitFor(() => expect(h.listFiles).toHaveBeenCalledTimes(2))
        await act(async () => initial.resolve({ path: "/home/user", file_info_list: [], total_count: 0 }))
        expect(screen.queryByText("/home/user")).toBeNull()
        expect(screen.getByText("My Computer")).toBeInTheDocument()
    })
})
