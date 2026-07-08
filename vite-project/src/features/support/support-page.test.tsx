import { describe, it, expect, vi, afterEach } from "vitest"
import { render, screen } from "@testing-library/react"

// Real en-US locale so assertions match what users see.
vi.mock("react-i18next", () => import("@/test-utils/i18n-mock").then((m) => m.reactI18nextMock()))
vi.mock("@/hooks/use-toast", () => ({ useToast: () => ({ toast: () => undefined }) }))

const useSupportStatus = vi.fn()
vi.mock("@/services/hooks/supportController/useSupportStatus", () => ({
    useSupportStatus: (opts: unknown) => useSupportStatus(opts),
}))
vi.mock("@/services/hooks/supportController/useStartSupport", () => ({
    useStartSupport: () => ({ mutateAsync: vi.fn(), isPending: false }),
}))
vi.mock("@/services/hooks/supportController/useStopSupport", () => ({
    useStopSupport: () => ({ mutateAsync: vi.fn(), isPending: false }),
}))

import { SupportPage } from "./support-page"

afterEach(() => {
    vi.clearAllMocks()
})

describe("SupportPage", () => {
    it("renders the page heading and embeds the support-code action", () => {
        useSupportStatus.mockReturnValue({ data: { data: { active: false } }, refetch: vi.fn() })
        render(<SupportPage />)
        expect(screen.getByRole("heading", { name: "Remote support" })).toBeInTheDocument()
        // The support-code card is embedded as the page's primary content.
        expect(screen.getByText("Get a support code")).toBeInTheDocument()
    })
})
