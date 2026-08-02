import { render, screen } from "@testing-library/react"
import { describe, expect, it, vi } from "vitest"

import { AsyncButton } from "@/components/async-button"

describe("AsyncButton", () => {
    it("renders the normal label while idle", () => {
        render(<AsyncButton pending={false}>Save</AsyncButton>)

        const button = screen.getByRole("button", { name: "Save" })
        expect(button).toBeEnabled()
        expect(button).not.toHaveAttribute("aria-busy")
        expect(button.querySelector("svg")).toBeNull()
    })

    it("shows the pending label and exposes busy semantics", () => {
        render(
            <AsyncButton pending pendingLabel="Saving">
                Save
            </AsyncButton>,
        )

        const button = screen.getByRole("button", { name: "Saving" })
        expect(button).toBeDisabled()
        expect(button).toHaveAttribute("aria-busy", "true")
        expect(button.querySelector("svg")).toHaveClass("animate-spin")
    })

    it("preserves a caller-provided disabled state", () => {
        render(
            <AsyncButton pending={false} disabled>
                Save
            </AsyncButton>,
        )

        expect(screen.getByRole("button", { name: "Save" })).toBeDisabled()
    })

    it("does not invoke click handlers while pending", () => {
        const onClick = vi.fn()
        render(
            <AsyncButton pending onClick={onClick} aria-label="Refresh">
                Refresh
            </AsyncButton>,
        )

        screen.getByRole("button", { name: "Refresh" }).click()
        expect(onClick).not.toHaveBeenCalled()
    })
})
