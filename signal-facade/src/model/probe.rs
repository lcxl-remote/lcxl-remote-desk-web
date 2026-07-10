//! Shared contract for the zero-side-effect signaling connection-verify probe.
//!
//! A desk-server (or the onboarding wizard) cannot open a browser WebSocket to an
//! arbitrary signaling host to check reachability + credentials — mixed-content /
//! CORS block it — so the check goes through a backend that connects to the
//! signaling endpoint with `?probe=1`. On that flag the signaling endpoint
//! short-circuits **after** authenticating the token but **before** any side
//! effect (no WebSocket upgrade, no device registration, no presence / quota, no
//! `last_used_at` write) and replies with the [`SIGNALING_PROBE_HEADER`] marker:
//!
//!   - token valid   → `200` + marker
//!   - token invalid / absent → `401` + marker
//!
//! The marker proves the response really came from a desk signaling endpoint
//! rather than an arbitrary HTTP server that happens to answer, so the caller
//! only treats "marker present + 200" as an authenticated probe. Both the
//! open-source `signal` and the enterprise `manager` implement the identical
//! behavior so control ends verify either the same way.

/// Response header a signaling endpoint sets on a `?probe=1` probe response
/// (present on both the 200 and 401 outcomes).
pub const SIGNALING_PROBE_HEADER: &str = "X-Desk-Signaling-Probe";

/// Value carried by [`SIGNALING_PROBE_HEADER`].
pub const SIGNALING_PROBE_HEADER_VALUE: &str = "1";

/// Whether a raw query string requests a zero-side-effect signaling probe
/// (`probe=1`). Parsing the raw query keeps the flag independent of the
/// `VersionInfo` deserialization so an unknown extra parameter never disturbs it.
pub fn is_probe_query(query_string: &str) -> bool {
    query_string.split('&').any(|pair| pair == "probe=1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_probe_flag_among_other_params() {
        assert!(is_probe_query("probe=1"));
        assert!(is_probe_query("token=abc&probe=1"));
        assert!(is_probe_query("probe=1&token=abc"));
    }

    #[test]
    fn ignores_absent_or_other_values() {
        assert!(!is_probe_query(""));
        assert!(!is_probe_query("token=abc"));
        assert!(!is_probe_query("probe=0"));
        assert!(!is_probe_query("probe=true"));
        // A different key that merely contains "probe" must not match.
        assert!(!is_probe_query("myprobe=1"));
    }
}
