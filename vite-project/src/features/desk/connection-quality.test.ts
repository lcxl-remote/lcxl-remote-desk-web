import { describe, it, expect } from "vitest"
import { connectionQuality, samplePacketLossPercent } from "./connection-quality"

describe("connectionQuality", () => {
    it("reports good on low loss and low latency", () => {
        expect(connectionQuality(0, 30)).toBe("good")
        expect(connectionQuality(1, 100)).toBe("good")
        // A fresh connection with no measurement yet reads good, not poor.
        expect(connectionQuality(0, 0)).toBe("good")
    })

    it("reports fair on moderate loss or latency", () => {
        expect(connectionQuality(2, 100)).toBe("fair")
        expect(connectionQuality(0, 200)).toBe("fair")
        expect(connectionQuality(3, 249)).toBe("fair")
    })

    it("reports poor once loss or latency crosses the degrade point", () => {
        expect(connectionQuality(4, 100)).toBe("poor")
        expect(connectionQuality(0, 300)).toBe("poor")
        expect(connectionQuality(10, 500)).toBe("poor")
    })
})

describe("samplePacketLossPercent", () => {
    it("reports the ratio over the window, not since the stream started", () => {
        expect(
            samplePacketLossPercent(
                { packetsLost: 100, packetsReceived: 1000 },
                { packetsLost: 105, packetsReceived: 1095 },
            ),
        ).toBe(5)
    })

    it("never reports a negative ratio when reordered packets arrive late", () => {
        // `packetsLost` walked back by one (a packet previously counted as lost
        // turned up) while five packets arrived. Differencing without clamping
        // gives -1 / (-1 + 5) = -25%, which is exactly what a weak link used to
        // render — in green, since the panel thresholds only look upward.
        expect(
            samplePacketLossPercent(
                { packetsLost: 40, packetsReceived: 900 },
                { packetsLost: 39, packetsReceived: 905 },
            ),
        ).toBe(0)
    })

    it("reports a genuine zero-loss window as 0, not as absent", () => {
        // Distinct from `null`: the caller must be able to clear a stale
        // non-zero reading once the link recovers.
        expect(
            samplePacketLossPercent(
                { packetsLost: 12, packetsReceived: 500 },
                { packetsLost: 12, packetsReceived: 560 },
            ),
        ).toBe(0)
    })

    it("reports no measurement without a baseline", () => {
        expect(
            samplePacketLossPercent(null, { packetsLost: 7, packetsReceived: 700 }),
        ).toBeNull()
    })

    it("reports no measurement for an empty window", () => {
        expect(
            samplePacketLossPercent(
                { packetsLost: 3, packetsReceived: 100 },
                { packetsLost: 3, packetsReceived: 100 },
            ),
        ).toBeNull()
    })

    it("survives counters that reset under it", () => {
        // A renegotiation that swaps the SSRC restarts both counters. Clamping
        // keeps the sample sane; the window simply carries no measurement.
        expect(
            samplePacketLossPercent(
                { packetsLost: 500, packetsReceived: 90000 },
                { packetsLost: 0, packetsReceived: 0 },
            ),
        ).toBeNull()
    })

    it("keeps the ratio within [0, 100]", () => {
        expect(
            samplePacketLossPercent(
                { packetsLost: 0, packetsReceived: 0 },
                { packetsLost: 50, packetsReceived: 0 },
            ),
        ).toBe(100)
    })
})
