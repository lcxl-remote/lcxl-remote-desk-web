import { describe, it, expect, vi, afterEach } from "vitest"
import { render, screen, fireEvent } from "@testing-library/react"

// Real en-US locale so assertions match the copy users see.
vi.mock("react-i18next", () => import("@/test-utils/i18n-mock").then((m) => m.reactI18nextMock()))

import { AgreementConsent } from "./legal-agreement"

afterEach(() => {
    vi.clearAllMocks()
})

describe("AgreementConsent", () => {
    it("renders the consent sentence with the two document links", () => {
        render(<AgreementConsent checked={false} onCheckedChange={() => undefined} />)
        expect(screen.getByText(/I have read and agree to the/)).toBeInTheDocument()
        expect(screen.getByRole("button", { name: "Terms of Service" })).toBeInTheDocument()
        expect(screen.getByRole("button", { name: "Privacy Policy" })).toBeInTheDocument()
    })

    it("reports checked=true when the box is toggled on", () => {
        const onCheckedChange = vi.fn()
        render(<AgreementConsent checked={false} onCheckedChange={onCheckedChange} />)
        fireEvent.click(screen.getByRole("checkbox"))
        expect(onCheckedChange).toHaveBeenCalledWith(true)
    })

    it("opens the Terms of Service dialog without toggling the checkbox", () => {
        const onCheckedChange = vi.fn()
        render(<AgreementConsent checked={false} onCheckedChange={onCheckedChange} />)
        fireEvent.click(screen.getByRole("button", { name: "Terms of Service" }))
        // Section 1 heading is unique to the terms document.
        expect(screen.getByText("1. Service Description")).toBeInTheDocument()
        // Clicking a document link must not flip the consent state.
        expect(onCheckedChange).not.toHaveBeenCalled()
    })

    it("opens the Privacy Policy dialog with its own content", () => {
        render(<AgreementConsent checked={false} onCheckedChange={() => undefined} />)
        fireEvent.click(screen.getByRole("button", { name: "Privacy Policy" }))
        expect(screen.getByText("1. Information We Collect")).toBeInTheDocument()
    })
})
