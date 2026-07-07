import { describe, it, expect, vi, afterEach } from "vitest"
import { render, screen } from "@testing-library/react"

// Real en-US locale so assertions match what users see.
vi.mock("react-i18next", () => import("@/test-utils/i18n-mock").then((m) => m.reactI18nextMock()))
vi.mock("@/hooks/use-toast", () => ({ useToast: () => ({ toast: () => undefined }) }))

const useSupportStatus = vi.fn()
const startMutation = { mutateAsync: vi.fn(), isPending: false }
const stopMutation = { mutateAsync: vi.fn(), isPending: false }
vi.mock("@/services/hooks/supportController/useSupportStatus", () => ({
    useSupportStatus: (opts: unknown) => useSupportStatus(opts),
}))
vi.mock("@/services/hooks/supportController/useStartSupport", () => ({
    useStartSupport: () => startMutation,
}))
vi.mock("@/services/hooks/supportController/useStopSupport", () => ({
    useStopSupport: () => stopMutation,
}))

import { SupportCodeCard } from "./support-code-card"

function withStatus(data: { active: boolean; code?: string | null; expires_at?: number | null }) {
    useSupportStatus.mockReturnValue({ data: { data }, refetch: vi.fn() })
}

afterEach(() => {
    vi.clearAllMocks()
})

describe("SupportCodeCard", () => {
    it("offers to get a code when no session is active", () => {
        withStatus({ active: false })
        render(<SupportCodeCard />)
        expect(screen.getByText("Get a support code")).toBeInTheDocument()
        expect(screen.queryByText("End support")).not.toBeInTheDocument()
    })

    it("shows the issued code and an end-support action", () => {
        withStatus({
            active: true,
            code: "ABCDEFGHJK",
            expires_at: Math.floor(Date.now() / 1000) + 300,
        })
        render(<SupportCodeCard />)
        expect(screen.getByText("ABCDEFGHJK")).toBeInTheDocument()
        expect(screen.getByText("End support")).toBeInTheDocument()
        // The "get a code" affordance is gone once a session is live.
        expect(screen.queryByText("Get a support code")).not.toBeInTheDocument()
    })

    it("shows an issuing state while the code is still pending", () => {
        withStatus({ active: true, code: null })
        render(<SupportCodeCard />)
        expect(screen.getByText("Requesting a code…")).toBeInTheDocument()
    })
})
