import { describe, it, expect, vi, beforeEach } from "vitest"
import { render, screen } from "@testing-library/react"

// i18n: real en-US locale so assertions match what users see.
vi.mock("react-i18next", () => import("@/test-utils/i18n-mock").then((m) => m.reactI18nextMock()))

const turnInfo: {
    data: { data: Record<string, unknown> } | undefined
    isLoading: boolean
    error: unknown
} = { data: undefined, isLoading: false, error: undefined }

/** The options the card asked the query with, so the polling policy is testable. */
let queryOptions: { query?: { refetchInterval?: unknown } } | undefined

vi.mock("@/services/hooks/turnController/useGetTurnInfo", () => ({
    useGetTurnInfo: (options: { query?: { refetchInterval?: unknown } }) => {
        queryOptions = options
        return turnInfo
    },
}))

import { TurnRuntimeStatus } from "./turn-runtime-status"

function info(overrides: Record<string, unknown>) {
    return {
        data: {
            state: "running",
            software: "test",
            interfaces: [],
            rejected_interfaces: [],
            uptime_secs: null,
            last_error: null,
            ...overrides,
        },
    }
}

beforeEach(() => {
    turnInfo.data = undefined
    turnInfo.isLoading = false
    turnInfo.error = undefined
    queryOptions = undefined
})

/** Ask the card's own policy what it would do with a given state. */
function pollFor(state: string): unknown {
    render(<TurnRuntimeStatus />)
    const refetchInterval = queryOptions?.query?.refetchInterval as (query: {
        state: { data: unknown }
    }) => unknown
    return refetchInterval({ state: { data: info({ state }) } })
}

describe("TurnRuntimeStatus", () => {
    /// "Not relaying" is the answer that most needs explaining, so each reason
    /// gets its own wording. A single "unavailable" would leave the operator
    /// unable to tell a switch they flipped from an address they never gave.
    it("explains each reason a host is not relaying", () => {
        const cases: [string, RegExp, RegExp][] = [
            ["disabled", /^Switched off$/, /neither relay nor STUN/i],
            ["unsupported", /^Not available in this mode$/, /does not run a TURN service/i],
            ["not-configured", /^Not configured$/, /no usable interface/i],
            ["failed", /^Failed to start$/, /could not start and is being retried/i],
        ]
        for (const [state, badge, detail] of cases) {
            turnInfo.data = info({ state })
            const { unmount } = render(<TurnRuntimeStatus />)
            expect(screen.getByText(badge), state).toBeInTheDocument()
            expect(screen.getByText(detail), state).toBeInTheDocument()
            unmount()
        }
    })

    /// A save returns before the host has bound a socket, so the read right
    /// after one lands mid-start. Calling that "failed to start" would report a
    /// failure for every save, and the operator has nothing to fix.
    it("reports a start that has not finished as under way, not as a failure", () => {
        turnInfo.data = info({ state: "starting" })
        render(<TurnRuntimeStatus />)

        expect(screen.getByText(/^Starting$/)).toBeInTheDocument()
        expect(screen.getByText(/is starting and is not relaying yet/i)).toBeInTheDocument()
        expect(screen.queryByText(/^Failed to start$/)).toBeNull()
    })

    /// The card is read once per mount, while the states above are left by the
    /// host on its own. Without asking again, a relay that comes up seconds
    /// after a save is reported as starting or failing for as long as the page
    /// stays open.
    it("keeps asking while the host is still settling, and stops when it has", () => {
        expect(pollFor("starting")).toBe(3000)
        expect(pollFor("failed")).toBe(3000)
        for (const settled of ["running", "disabled", "unsupported", "not-configured"]) {
            expect(pollFor(settled), settled).toBe(false)
        }
    })

    /// A failed start carries the only actionable detail there is, so it is
    /// shown rather than collapsed into "failed".
    it("shows why a start failed", () => {
        turnInfo.data = info({ state: "failed", last_error: "Bind failed: address in use" })
        render(<TurnRuntimeStatus />)

        expect(screen.getByText("Bind failed: address in use")).toBeInTheDocument()
    })

    /// A running relay reports what it serves — the addresses actually bound,
    /// which is not the same list as the one saved in the form below.
    it("reports the interfaces a running relay serves", () => {
        turnInfo.data = info({
            state: "running",
            interfaces: [{ transport: "udp", listen: "0.0.0.0:3478", external: "203.0.113.7:3478" }],
            uptime_secs: 7200,
        })
        render(<TurnRuntimeStatus />)

        expect(screen.getByText(/0\.0\.0\.0:3478.*203\.0\.113\.7:3478/)).toBeInTheDocument()
        expect(screen.getByText("2 hr")).toBeInTheDocument()
    })

    /// The case that used to be invisible: the relay is up, so everything looks
    /// healthy, while one configured address is quietly not in use.
    it("reports refused entries even while the relay is up", () => {
        turnInfo.data = info({
            state: "running",
            interfaces: [{ transport: "udp", listen: "0.0.0.0:3478", external: "203.0.113.7:3478" }],
            rejected_interfaces: [
                {
                    index: 1,
                    interface: { transport: "tcp", listen: "0.0.0.0:3478", external: "203.0.113.7:3478" },
                    fault: "transport-not-served",
                    detail: 'transport TCP is not relayed; only UDP is served',
                },
            ],
        })
        render(<TurnRuntimeStatus />)

        expect(screen.getByText(/these entries are not in use/i)).toBeInTheDocument()
        expect(screen.getByText(/only UDP is relayed/i)).toBeInTheDocument()
        // Numbered from one for a human reading a list, not from the wire index.
        expect(screen.getByText(/#2 TCP/)).toBeInTheDocument()
    })

    /// Every fault the server can report has to reach a localized line; a
    /// missing case would render the raw key.
    it("has wording for every fault the server can report", () => {
        const faults = [
            "transport-not-served",
            "listen-not-an-address",
            "external-not-an-address",
            "external-not-dialable",
        ]
        for (const fault of faults) {
            turnInfo.data = info({
                state: "not-configured",
                rejected_interfaces: [
                    {
                        index: 0,
                        interface: { transport: "udp", listen: "x", external: "y" },
                        fault,
                        detail: "detail text",
                    },
                ],
            })
            const { unmount } = render(<TurnRuntimeStatus />)
            expect(screen.queryByText(/pages\.turn\.runtime\.fault/), fault).toBeNull()
            unmount()
        }
    })

    /// The endpoint answers in every startup mode, so a failure here is about
    /// reaching the server — not a reason to imply TURN is broken.
    it("treats an unreachable endpoint as a connection problem", () => {
        turnInfo.error = new Error("network")
        render(<TurnRuntimeStatus />)

        expect(screen.getByText(/check the connection to this server/i)).toBeInTheDocument()
    })
})
