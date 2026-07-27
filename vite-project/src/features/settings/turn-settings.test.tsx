import { describe, it, expect, vi, beforeEach } from "vitest"
import { render, screen, fireEvent, waitFor } from "@testing-library/react"

// i18n: real en-US locale so assertions match what users see.
vi.mock("react-i18next", () => import("@/test-utils/i18n-mock").then((m) => m.reactI18nextMock()))
vi.mock("@/hooks/use-toast", () => ({ useToast: () => ({ toast: () => undefined }) }))

// Mutable so each test can decide what the server answered before rendering.
const turnQuery: { data: { data: Record<string, unknown> } | undefined; isLoading: boolean } = {
    data: undefined,
    isLoading: false,
}
const updateTurnSettings = vi.fn(async () => ({}))

vi.mock("@/services/hooks/turnController/useQueryTurnSettings", () => ({
    useQueryTurnSettings: () => turnQuery,
}))
vi.mock("@/services/hooks/turnController/useUpdateTurnSettings", () => ({
    useUpdateTurnSettings: () => ({ mutateAsync: updateTurnSettings, isPending: false }),
}))
vi.mock("@/services/hooks/turnController/useRegenerateTurnSecret", () => ({
    useRegenerateTurnSecret: () => ({ mutateAsync: vi.fn(), isPending: false }),
}))

import { TurnSettings } from "./turn-settings"

const SAVED = {
    realm: "example.org",
    interfaces: [{ transport: "udp", listen: "0.0.0.0:3478", external: "1.2.3.4:3478" }],
    static_auth_secret: "server-side-secret",
    relay_min_port: 50000,
    relay_max_port: 50050,
}

function turnSwitch() {
    return screen.findByRole("switch", { name: /enable turn service/i })
}

beforeEach(() => {
    updateTurnSettings.mockClear()
    turnQuery.data = undefined
    turnQuery.isLoading = false
})

describe("TurnSettings — the TURN service switch", () => {
    /// The switch decides whether this host relays, and the backend default is
    /// on. A server that answers without the field (an older build, or a config
    /// that predates the switch) must not read as "the operator turned it off".
    it("shows the service as on when the server did not say otherwise", async () => {
        turnQuery.data = { data: { ...SAVED } }
        render(<TurnSettings />)

        await waitFor(async () => expect(await turnSwitch()).toBeChecked())
    })

    it("shows the service as off once the operator has turned it off", async () => {
        turnQuery.data = { data: { ...SAVED, enable_turn: false } }
        render(<TurnSettings />)

        await waitFor(async () => expect(await turnSwitch()).not.toBeChecked())
    })

    /// STUN is no longer separately switchable, so the form must not send a
    /// key for it — and it still has to carry through the fields it does not
    /// render, notably the server-held secret.
    it("saves the switch, keeps untouched fields, and sends nothing about STUN", async () => {
        turnQuery.data = { data: { ...SAVED, enable_turn: true } }
        render(<TurnSettings />)

        fireEvent.click(await turnSwitch())
        fireEvent.click(screen.getByRole("button", { name: /save settings/i }))

        await waitFor(() => expect(updateTurnSettings).toHaveBeenCalledTimes(1))
        const payload = (updateTurnSettings.mock.calls[0] as unknown as [{ data: Record<string, unknown> }])[0]
            .data
        expect(payload.enable_turn).toBe(false)
        expect(payload).not.toHaveProperty("enable_stun")
        expect(payload.static_auth_secret).toBe("server-side-secret")
        expect(payload.realm).toBe("example.org")
    })
})
