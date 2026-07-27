import { describe, it, expect, vi, beforeEach } from "vitest"
import { render, screen } from "@testing-library/react"

// i18n: real en-US locale so assertions match what users see.
vi.mock("react-i18next", () => import("@/test-utils/i18n-mock").then((m) => m.reactI18nextMock()))

const usage: { data: unknown; isLoading: boolean; error: unknown } = {
    data: undefined,
    isLoading: false,
    error: undefined,
}
const turnInfo: { data: { data: Record<string, unknown> } | undefined } = { data: undefined }

vi.mock("@/services/hooks/turnUsageController/useGetTurnUsage", () => ({
    useGetTurnUsage: () => usage,
}))
vi.mock("@/services/hooks/turnController/useGetTurnInfo", () => ({
    useGetTurnInfo: () => turnInfo,
}))

import { TurnUsagePage } from "./turn-usage"

const ROW = {
    deviceCode: "device-1",
    hourBucket: "2026-07-27T00:00:00Z",
    relayReceivedBytes: 10,
    relaySentBytes: 10,
    relayReceivedPkts: 1,
    relaySentPkts: 1,
    controlReceivedBytes: 0,
    controlSentBytes: 0,
    controlReceivedPkts: 0,
    controlSentPkts: 0,
}

beforeEach(() => {
    usage.data = { data: { items: [] } }
    usage.isLoading = false
    usage.error = undefined
    turnInfo.data = undefined
})

describe("TurnUsagePage — an empty history", () => {
    /// The page is reachable in every mode that keeps the history, so a host
    /// that never relays lands here and sees nothing. An empty chart alone
    /// reads as a broken page; the runtime state says it is working as intended.
    it("explains an empty range on a host that is not relaying", () => {
        turnInfo.data = { data: { state: "disabled" } }
        render(<TurnUsagePage />)

        expect(screen.getByText(/is not running, so no new relay traffic/i)).toBeInTheDocument()
    })

    /// A mode that never hosts TURN gets its own wording: there is no switch to
    /// look for, so pointing at the settings page would waste the operator's
    /// time.
    it("says so plainly where TURN is never hosted", () => {
        turnInfo.data = { data: { state: "unsupported" } }
        render(<TurnUsagePage />)

        expect(screen.getByText(/does not run a TURN service/i)).toBeInTheDocument()
    })

    /// A relay that is up and simply has no traffic in this range is not a
    /// problem to explain — the chart's own empty state already covers it.
    it("adds nothing when the relay is up", () => {
        turnInfo.data = { data: { state: "running" } }
        render(<TurnUsagePage />)

        expect(screen.queryByText(/no new relay traffic/i)).toBeNull()
    })

    /// The hint is about there being no relay, not about the range being empty,
    /// so data present means no hint whatever the runtime says.
    it("stays out of the way once there is data", () => {
        usage.data = { data: { items: [ROW] } }
        turnInfo.data = { data: { state: "disabled" } }
        render(<TurnUsagePage />)

        expect(screen.queryByText(/no new relay traffic/i)).toBeNull()
    })
})
