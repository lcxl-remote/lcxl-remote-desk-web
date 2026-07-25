import { describe, it, expect, vi } from "vitest"
import { render } from "@testing-library/react"
import { MemoryRouter } from "react-router-dom"

// i18n: real en-US locale, so the rendered card titles match production copy.
vi.mock("react-i18next", () => import("@/test-utils/i18n-mock").then((m) => m.reactI18nextMock()))

// Mutable server info so each case can pick the startup mode under test.
const h = vi.hoisted(() => ({
    startupMode: "default" as string,
    backgroundStart: null as unknown,
}))

vi.mock("@/services/hooks/systemController/useQueryServerInfo", () => ({
    useQueryServerInfo: () => ({
        data: {
            data: {
                startup_mode: h.startupMode,
                background_start: h.backgroundStart,
            },
        },
    }),
}))

import { SettingsOverview } from "./settings-overview"

function renderFor(startupMode: string) {
    h.startupMode = startupMode
    const { container } = render(
        <MemoryRouter>
            <SettingsOverview />
        </MemoryRouter>,
    )
    return container
}

const aiModelLink = (c: HTMLElement) => c.querySelector('a[href="/system/ai-model"]')
const aiPolicyLink = (c: HTMLElement) => c.querySelector('a[href="/system/ai-policy"]')

describe("SettingsOverview AI settings placement", () => {
    // The AI model config lives under the Signal category (the central brain
    // dials the model), so it renders when the signal section does and is hidden
    // on a desk-server-only edge.
    it("shows the AI model card in signaling mode (Signal category)", () => {
        const c = renderFor("signaling")
        expect(aiModelLink(c)).not.toBeNull()
        expect(aiPolicyLink(c)).toBeNull()
    })

    it("shows the AI model card in default mode", () => {
        const c = renderFor("default")
        expect(aiModelLink(c)).not.toBeNull()
        expect(aiPolicyLink(c)).not.toBeNull()
    })

    it("shows only the local AI policy card on a desk-server edge", () => {
        const c = renderFor("desk_server")
        expect(aiModelLink(c)).toBeNull()
        expect(aiPolicyLink(c)).not.toBeNull()
    })
})
