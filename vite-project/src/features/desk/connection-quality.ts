/**
 * Live connection-quality classification from WebRTC inbound stats.
 *
 * Backend-agnostic by construction: it reads only the browser's RTCStats
 * (packet loss + round-trip time), so it reports identically whether the session
 * runs against the open-source signal server or a manager instance.
 */

export type ConnectionQuality = "good" | "fair" | "poor"

/**
 * Classify connection quality. `packetLoss` is a percentage (0–100) and `rtt` is
 * in milliseconds, matching the shape produced by `useDeskRTC`'s stats loop. The
 * thresholds line up with the adaptive-quality loop's degrade point (>3% loss or
 * >200ms rtt), so the badge turns amber around the same time the encoder backs
 * off. A fresh connection with no measurement yet (rtt 0, no loss) reads "good".
 */
export function connectionQuality(packetLoss: number, rtt: number): ConnectionQuality {
    if (packetLoss <= 1 && rtt < 120) return "good"
    if (packetLoss <= 3 && rtt < 250) return "fair"
    return "poor"
}

/** The two inbound RTP counters a packet-loss sample is derived from. */
export interface PacketCounters {
    packetsLost: number
    packetsReceived: number
}

/**
 * Packet loss over one sampling window, as a percentage in `[0, 100]`.
 *
 * `packetsLost` is signed in the stats spec and is allowed to go *down*: loss is
 * inferred from RTP sequence numbers, so a packet that arrives out of order is
 * first counted as lost and then subtracted back once it lands. A weak link is
 * exactly where that happens, which is why the raw delta must never reach the
 * ratio — a negative reading is meaningless to a viewer and, worse, drags the
 * adaptive-quality average down into its "network is great" branch, which steps
 * the encoder toward a higher bitrate at the moment the link is struggling.
 *
 * Returns `null` when the window carries no measurement: no baseline yet (a
 * fresh PeerConnection), or nothing arrived and nothing was newly lost. The
 * caller keeps its previous reading rather than rendering a fabricated 0.
 */
export function samplePacketLossPercent(
    previous: PacketCounters | null,
    current: PacketCounters,
): number | null {
    if (previous === null) return null
    const lost = Math.max(0, current.packetsLost - previous.packetsLost)
    const received = Math.max(0, current.packetsReceived - previous.packetsReceived)
    const total = lost + received
    if (total <= 0) return null
    return Number(((lost / total) * 100).toFixed(2))
}
