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

// The page always renders the runtime status card; these tests are about the
// form, so the card is given a runtime that is up and has nothing to report.
const turnInfo: { data: { data: Record<string, unknown> } | undefined; isLoading: boolean } = {
    data: undefined,
    isLoading: false,
}

// No provider is mounted here, and what these tests check is which queries the
// page declares stale — not the cache doing it.
const invalidateQueries = vi.fn()
vi.mock("@tanstack/react-query", async (importOriginal) => ({
    ...(await importOriginal<Record<string, unknown>>()),
    useQueryClient: () => ({ invalidateQueries }),
}))

const regenerateSecret = vi.fn(async () => ({}))
const queryStatistics = vi.fn(async () => ({}))
const statisticsQuery: {
    data?: { data?: Record<string, unknown> }
    error?: Error
    isFetching: boolean
} = { isFetching: false }

vi.mock("@/services/hooks/turnController/useGetTurnInfo", () => ({
    useGetTurnInfo: () => turnInfo,
    getTurnInfoQueryKey: () => [{ url: "/api/turn/info" }],
}))
vi.mock("@/services/hooks/turnController/useQueryTurnSettings", () => ({
    useQueryTurnSettings: () => turnQuery,
}))
vi.mock("@/services/hooks/turnController/useUpdateTurnSettings", () => ({
    useUpdateTurnSettings: () => ({ mutateAsync: updateTurnSettings, isPending: false }),
}))
vi.mock("@/services/hooks/turnController/useRegenerateTurnSecret", () => ({
    useRegenerateTurnSecret: () => ({ mutateAsync: regenerateSecret, isPending: false }),
}))
vi.mock("@/services/hooks/turnController/useGetTurnSessionStatistics", () => ({
    useGetTurnSessionStatistics: () => ({ ...statisticsQuery, refetch: queryStatistics }),
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
    regenerateSecret.mockClear()
    invalidateQueries.mockClear()
    queryStatistics.mockClear()
    statisticsQuery.data = undefined
    statisticsQuery.error = undefined
    statisticsQuery.isFetching = false
    turnQuery.data = undefined
    turnQuery.isLoading = false
    turnInfo.data = {
        data: {
            state: "running",
            software: "test",
            interfaces: [],
            rejected_interfaces: [],
            uptime_secs: 1,
            last_error: null,
        },
    }
    turnInfo.isLoading = false
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

describe("TurnSettings — saving is the confirmation", () => {
    /// Saving restarts the relay and drops whatever it is carrying, and there is
    /// deliberately no extra dialog in the way. The cost therefore has to be
    /// stated where the action is taken, or the operator finds out by losing a
    /// session.
    it("states that saving interrupts relayed connections", () => {
        turnQuery.data = { data: { ...SAVED } }
        render(<TurnSettings />)

        expect(screen.getByText(/restarts the TURN service immediately/i)).toBeInTheDocument()
        expect(screen.getByText(/drops the connections currently being relayed/i)).toBeInTheDocument()
    })
})

describe("TurnSettings — the status card follows what was saved", () => {
    /// Both writes restart, stop or re-key the relay. The card polls only the
    /// states the host leaves by itself, so without this the operator who just
    /// switched TURN off goes on being told it is running.
    it("marks the runtime status stale after saving", async () => {
        turnQuery.data = { data: { ...SAVED } }
        render(<TurnSettings />)

        fireEvent.click(await turnSwitch())
        fireEvent.click(screen.getByRole("button", { name: /save settings/i }))

        await waitFor(() => expect(updateTurnSettings).toHaveBeenCalledTimes(1))
        expect(invalidateQueries).toHaveBeenCalledWith({
            queryKey: [{ url: "/api/turn/info" }],
        })
    })

    /// A rotated secret is adopted by restarting the runtime, so the card is
    /// describing a server that no longer exists until it is re-read.
    it("marks the runtime status stale after rotating the secret", async () => {
        turnQuery.data = { data: { ...SAVED } }
        render(<TurnSettings />)

        fireEvent.click(screen.getByRole("button", { name: /regenerate turn secret/i }))
        fireEvent.click(await screen.findByRole("button", { name: /^confirm$/i }))

        await waitFor(() => expect(regenerateSecret).toHaveBeenCalledTimes(1))
        expect(invalidateQueries).toHaveBeenCalledWith({
            queryKey: [{ url: "/api/turn/info" }],
        })
    })
})

describe("TurnSettings — interface addresses", () => {
    async function editExternal(value: string) {
        turnQuery.data = { data: { ...SAVED } }
        render(<TurnSettings />)
        const external = await screen.findByDisplayValue("1.2.3.4:3478")
        fireEvent.change(external, { target: { value } })
        fireEvent.click(screen.getByRole("button", { name: /save settings/i }))
    }

    /// The server refuses an address it cannot bind or advertise. Catching it
    /// here means the operator is told while looking at the field, instead of
    /// saving an entry that is silently never served.
    it("refuses an address the server would reject", async () => {
        await editExternal("relay.example.com:3478")

        expect(await screen.findByText(/peers can dial/i)).toBeInTheDocument()
        expect(updateTurnSettings).not.toHaveBeenCalled()
    })

    /// A wildcard address parses but names nothing dialable — the exact value an
    /// earlier build substituted when parsing failed.
    it("refuses a wildcard external address", async () => {
        await editExternal("0.0.0.0:3478")

        expect(await screen.findByText(/wildcard address/i)).toBeInTheDocument()
        expect(updateTurnSettings).not.toHaveBeenCalled()
    })

    /// IPv6 has to survive the form: brackets and all, it is a valid entry.
    it("accepts a bracketed IPv6 address", async () => {
        await editExternal("[2001:db8::1]:3478")

        await waitFor(() => expect(updateTurnSettings).toHaveBeenCalledTimes(1))
        const payload = (updateTurnSettings.mock.calls[0] as unknown as [{ data: Record<string, unknown> }])[0]
            .data
        expect((payload.interfaces as { external: string }[])[0].external).toBe("[2001:db8::1]:3478")
    })
})

describe("TurnSettings — session diagnostics", () => {
    it("queries a known address and renders relay/control counters", async () => {
        turnQuery.data = { data: { ...SAVED } }
        statisticsQuery.data = {
            data: {
                relay: { received_bytes: 10, send_bytes: 20, received_pkts: 1, send_pkts: 2 },
                control: { received_bytes: 30, send_bytes: 40, received_pkts: 3, send_pkts: 4 },
                error_pkts: 5,
            },
        }
        render(<TurnSettings />)

        fireEvent.click(screen.getByRole("button", { name: /advanced statistics lookup/i }))
        fireEvent.change(screen.getByLabelText("Client IP:port"), {
            target: { value: "203.0.113.10:54321" },
        })
        fireEvent.click(screen.getByRole("button", { name: /^query$/i }))

        await waitFor(() => expect(queryStatistics).toHaveBeenCalledTimes(1))
        expect(screen.getByText("Relay traffic")).toBeInTheDocument()
        expect(screen.getByText("Control traffic")).toBeInTheDocument()
        expect(screen.getByText("5 error packet(s)")).toBeInTheDocument()
    })
})
