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
