import { fireEvent, render, screen, waitFor } from "@testing-library/react"
import { MemoryRouter, Route, Routes } from "react-router-dom"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"

vi.mock("react-i18next", () =>
    import("@/test-utils/i18n-mock").then((module) => module.reactI18nextMock()),
)

// What the page looks like when the data channel could not be established.
//
// The production failure took the whole page down: nothing listed, one generic
// timeout, and no way to tell a broken relay from a broken host. Here the listing
// is present, the transfer controls are the only thing withdrawn, and the banner
// names the cause — so this asserts the degraded state is genuinely usable rather
// than merely non-fatal.

const h = vi.hoisted(() => ({
    listFiles: vi.fn(),
    toast: vi.fn(),
    querySystemInfo: vi.fn().mockResolvedValue(null),
    closeConnection: vi.fn(),
    prepareTransfers: vi.fn(),
    downloadFile: vi.fn(),
}))

const FAILURE = {
    kind: "channel-timeout" as const,
    message: "File transfer channel timed out",
    diagnostics: {
        iceServerUrls: ["turn:relay.example:3478"],
        candidateCounts: { host: 2, srflx: 1, prflx: 0, relay: 0, unknown: 0 },
        gatheringState: "gathering",
        iceConnectionState: "checking",
        sessionMs: 780,
        dataChannelMs: 20_000,
        failedStage: "dataChannel" as const,
    },
}

vi.mock("@/services/hooks/connectionController/useListConnections", () => ({
    useListConnections: () => ({ data: [] }),
}))

vi.mock("./use-file-transfer", async (importOriginal) => ({
    ...(await importOriginal<typeof import("./use-file-transfer")>()),
    useFileTransfer: () => ({
        transfers: [],
        downloadFile: h.downloadFile,
        uploadFile: vi.fn(),
        cancelTransfer: vi.fn(),
        removeTransfer: vi.fn(),
        listFiles: h.listFiles,
        deleteFile: vi.fn(),
        querySystemInfo: h.querySystemInfo,
        closeConnection: h.closeConnection,
        prepareTransfers: h.prepareTransfers,
        channelStatus: "failed" as const,
        channelFailure: FAILURE,
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

describe("FileList with an unavailable transfer channel", () => {
    let writeText: ReturnType<typeof vi.fn>

    beforeEach(() => {
        h.listFiles.mockReset()
        h.listFiles.mockResolvedValue({
            file_info_list: [{ name: "a.txt", path: "/a.txt", is_dir: false, size: 12, modified: 0 }],
            total_count: 1,
        })
        writeText = vi.fn().mockResolvedValue(undefined)
        Object.defineProperty(navigator, "clipboard", {
            value: { writeText },
            configurable: true,
        })
    })

    afterEach(() => {
        vi.restoreAllMocks()
    })

    it("still lists files, withdraws only the transfer controls, and names the likely cause", async () => {
        render(
            <MemoryRouter initialEntries={["/files/connection-1"]}>
                <Routes>
                    <Route path="/files/:id" element={<FileList />} />
                </Routes>
            </MemoryRouter>,
        )

        // The listing is there — this is what the outage denied the user entirely.
        expect(await screen.findByText("a.txt")).toBeInTheDocument()

        // The banner explains the stage that failed and what still works.
        expect(screen.getByText("File transfer channel unavailable")).toBeInTheDocument()
        expect(
            screen.getByText("Timed out establishing the transfer channel to the device."),
        ).toBeInTheDocument()
        expect(
            screen.getByText(/Browsing and deleting still work/),
        ).toBeInTheDocument()

        // A configured relay with no relay candidate is the finding that matters,
        // and the banner leads with it rather than making the user infer it.
        expect(screen.getByText(/no relay candidate could be gathered/)).toBeInTheDocument()

        // Transfers are withdrawn; nothing else is.
        expect(screen.getByRole("button", { name: /Upload/ })).toBeDisabled()
        const downloadButton = screen.getByRole("button", {
            name: "File transfer channel unavailable",
        })
        expect(downloadButton).toBeDisabled()
        fireEvent.click(downloadButton)
        expect(h.downloadFile).not.toHaveBeenCalled()
        expect(screen.getByRole("button", { name: "Refresh" })).toBeEnabled()

        // Retrying is offered, and asks for exactly the thing that failed.
        fireEvent.click(screen.getByRole("button", { name: /Retry connection/ }))
        expect(h.prepareTransfers).toHaveBeenCalled()

        // The diagnostics can be handed to whoever runs the relay.
        fireEvent.click(screen.getByRole("button", { name: /Copy diagnostics/ }))
        await waitFor(() => expect(writeText).toHaveBeenCalled())
        const copied = writeText.mock.calls[0][0] as string
        expect(copied).toContain("ice_servers: turn:relay.example:3478")
        expect(copied).toContain("relay=0")
        expect(copied).toContain("failed_stage: dataChannel")
    })
})
