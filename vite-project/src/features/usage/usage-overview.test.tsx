import { describe, it, expect, vi } from "vitest"
import { render } from "@testing-library/react"
import { MemoryRouter } from "react-router-dom"

// i18n: real en-US locale, so the rendered card titles match production copy.
vi.mock("react-i18next", () => import("@/test-utils/i18n-mock").then((m) => m.reactI18nextMock()))

import { UsageOverview } from "./usage-overview"

function renderOverview() {
    const { container } = render(
        <MemoryRouter>
            <UsageOverview />
        </MemoryRouter>,
    )
    return container
}

describe("UsageOverview", () => {
    // TURN traffic and AI token usage now live on their own top-level page
    // (sibling of Settings), each reachable as a card under /usage.
    it("links the TURN usage card to /usage/turn", () => {
        const c = renderOverview()
        expect(c.querySelector('a[href="/usage/turn"]')).not.toBeNull()
    })

    it("links the AI token usage card to /usage/model", () => {
        const c = renderOverview()
        expect(c.querySelector('a[href="/usage/model"]')).not.toBeNull()
    })

    it("links the retention config card to /usage/retention", () => {
        const c = renderOverview()
        expect(c.querySelector('a[href="/usage/retention"]')).not.toBeNull()
    })

    it("does not reuse the old settings-scoped routes", () => {
        const c = renderOverview()
        expect(c.querySelector('a[href="/system/turn-usage"]')).toBeNull()
        expect(c.querySelector('a[href="/system/model-usage"]')).toBeNull()
    })
})
