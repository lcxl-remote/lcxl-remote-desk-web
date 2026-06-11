//! REMB-driven adaptive bitrate-cap controller (daemon side).
//!
//! The encoders run quality-first (CRF / CQ): crisp text and near-zero
//! bitrate on a static desktop, but an *unbounded* instantaneous
//! bitrate under heavy motion (scrolling, video playback, window
//! drags). Spikes beyond the link capacity queue up, inflate RTT and
//! drop packets — the dominant cause of freezes. This module closes
//! the loop: the browser's RTCP REMB packets (receiver-estimated
//! maximum bitrate, negotiated via `goog-remb`) feed a per-connection
//! controller that emits **bitrate-cap directives** towards the worker
//! (`UpdateMediaSettingsPayload.bitrate_kbps`). The cap only bounds
//! spikes; steady-state quality is untouched.
//!
//! Two cooperating loops exist: this inner loop (fast, seconds) trims
//! rate spikes without touching the quality knob, while the browser's
//! pre-existing adaptive-quality loop (slow, tens of seconds) remains
//! the fallback that trades visual quality when bandwidth stays
//! insufficient.
//!
//! ## Concurrency contract
//!
//! All decisions and the IPC sends they trigger must happen while
//! holding [`AdaptiveBitrateShared::state`] — the RTCP task's REMB
//! path and the settings handler's disable path both lock, decide,
//! `send_to_worker(..).await`, then commit before unlocking. Lock-held
//! ordering plus the FIFO event pipe guarantees the worker observes
//! directives in decision order, so a stale `SetCap` can never land
//! after the `Clear` emitted when the feature is switched off.
//!
//! ## Two-phase commit
//!
//! `decide_on_remb` is pure; state is only advanced by [`
//! AdaptiveBitrateState::commit`] *after* the IPC send succeeded. A
//! failed send therefore leaves `current_cap_kbps` / `last_sent`
//! untouched and the next REMB re-decides from scratch instead of
//! being suppressed by hysteresis. The `enabled` flag is the one
//! exception: [`AdaptiveBitrateState::set_enabled_and_decide_clear`]
//! flips it immediately regardless of how the Clear send fares,
//! because stopping further cap emissions must not depend on IPC
//! health (a failed send implies a torn-down pipe, and a worker
//! restart rebuilds encoders at their initial ceiling anyway).

use std::time::{Duration, Instant};

use tokio::sync::Mutex;

/// Fraction of the REMB estimate handed to the video encoder; the
/// remainder is headroom for audio, retransmissions and the cursor /
/// clipboard DataChannels.
const HEADROOM: f64 = 0.85;

/// Floor for any emitted cap. Keeps 1080p text legible even when the
/// receiver-side estimate decays on a near-static desktop, so the
/// first motion burst is never throttled below usability.
pub const MIN_CAP_KBPS: u32 = 1_000;

/// Ceiling above which the controller considers the link unconstrained
/// and clears the cap instead of chasing the estimate.
pub const MAX_CAP_KBPS: u32 = 50_000;

/// Minimum interval between two emitted directives (REMB itself
/// arrives at roughly 1 Hz).
const MIN_SEND_INTERVAL: Duration = Duration::from_secs(1);

/// Relative change below which a new target is suppressed to avoid
/// directive churn.
const HYSTERESIS: f64 = 0.15;

/// Relative drop that bypasses `MIN_SEND_INTERVAL` — congestion onset
/// must tighten the cap immediately.
const URGENT_DROP: f64 = 0.5;

/// Maximum relative growth of the cap per second while recovering.
/// Falling follows the estimate immediately; rising is rate-limited so
/// one optimistic REMB sample cannot re-open the floodgates (AIMD
/// style). Tuned against live REMB traces; revisit together with the
/// REMB observation notes in the implementation plan.
const RAISE_PER_SECOND: f64 = 0.5;

/// A cap change the caller must ship to the worker as
/// `UpdateMediaSettingsPayload.bitrate_kbps`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapDirective {
    /// Cap the encoder at this many kbps (wire form `Some(kbps)`).
    SetCap(u32),
    /// Remove the cap; the encoder returns to its initial ceiling
    /// (wire form `Some(0)`).
    Clear,
}

impl CapDirective {
    /// Wire encoding for `UpdateMediaSettingsPayload.bitrate_kbps`.
    pub fn wire_kbps(&self) -> u32 {
        match self {
            CapDirective::SetCap(kbps) => *kbps,
            CapDirective::Clear => 0,
        }
    }
}

/// Single source of truth for one connection's adaptive-bitrate state:
/// the enabled flag and the controller fields live together so there
/// is exactly one place to consult (no split-brain between a shared
/// atomic and controller-internal state).
#[derive(Debug)]
pub struct AdaptiveBitrateState {
    enabled: bool,
    /// Cap the worker is known to run with (`None` = initial ceiling).
    /// Only advanced by `commit`, i.e. after a successful IPC send.
    current_cap_kbps: Option<u32>,
    /// Instant of the last successfully sent directive.
    last_sent: Option<Instant>,
}

impl AdaptiveBitrateState {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            current_cap_kbps: None,
            last_sent: None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn current_cap_kbps(&self) -> Option<u32> {
        self.current_cap_kbps
    }

    /// Pure decision on a fresh REMB estimate — mutates nothing.
    /// Returns the directive to send, or `None` to stay silent.
    pub fn decide_on_remb(&self, now: Instant, remb_bps: f64) -> Option<CapDirective> {
        if !self.enabled || !remb_bps.is_finite() || remb_bps <= 0.0 {
            return None;
        }
        let target_kbps = ((remb_bps / 1000.0 * HEADROOM) as u32).clamp(MIN_CAP_KBPS, MAX_CAP_KBPS);

        let interval_ok = match self.last_sent {
            Some(t) => now.saturating_duration_since(t) >= MIN_SEND_INTERVAL,
            None => true,
        };

        match self.current_cap_kbps {
            None => {
                // Uncapped: only engage when the estimate actually
                // constrains us. An unconstrained link clamps to
                // MAX_CAP_KBPS and stays silent.
                if target_kbps >= MAX_CAP_KBPS || !interval_ok {
                    return None;
                }
                Some(CapDirective::SetCap(target_kbps))
            }
            Some(cur) => {
                let cur_f = cur as f64;
                if (target_kbps as f64) < cur_f {
                    // Falling: congestion. Urgent drops bypass the
                    // send-interval limiter; smaller dips honour both
                    // hysteresis and the interval.
                    let drop_ratio = (cur_f - target_kbps as f64) / cur_f;
                    if drop_ratio >= URGENT_DROP {
                        return Some(CapDirective::SetCap(target_kbps));
                    }
                    if drop_ratio < HYSTERESIS || !interval_ok {
                        return None;
                    }
                    Some(CapDirective::SetCap(target_kbps))
                } else {
                    // Rising: recovery. Rate-limit the growth so one
                    // optimistic sample cannot blow past the link;
                    // once the rate-limited target clears MAX_CAP_KBPS
                    // the link is no longer the constraint and the cap
                    // is removed entirely.
                    if !interval_ok {
                        return None;
                    }
                    let elapsed = self
                        .last_sent
                        .map(|t| now.saturating_duration_since(t).as_secs_f64())
                        .unwrap_or(1.0);
                    let allowed = cur_f * (1.0 + RAISE_PER_SECOND * elapsed);
                    let raised = (target_kbps as f64).min(allowed) as u32;
                    if raised >= MAX_CAP_KBPS {
                        return Some(CapDirective::Clear);
                    }
                    if (raised as f64 - cur_f) / cur_f < HYSTERESIS {
                        return None;
                    }
                    Some(CapDirective::SetCap(raised))
                }
            }
        }
    }

    /// Stateful (the name says so on purpose): flips `enabled`
    /// immediately — further REMB decisions stop right away regardless
    /// of how the subsequent IPC send fares — and returns the `Clear`
    /// the caller must ship when a cap is currently applied.
    pub fn set_enabled_and_decide_clear(&mut self, new_enabled: bool) -> Option<CapDirective> {
        let was_enabled = self.enabled;
        self.enabled = new_enabled;
        if was_enabled && !new_enabled && self.current_cap_kbps.is_some() {
            Some(CapDirective::Clear)
        } else {
            None
        }
    }

    /// Advances the controller state after the directive was
    /// *successfully* sent to the worker. Never call on a failed send
    /// — see the two-phase-commit contract in the module docs.
    pub fn commit(&mut self, directive: CapDirective, now: Instant) {
        match directive {
            CapDirective::SetCap(kbps) => self.current_cap_kbps = Some(kbps),
            CapDirective::Clear => self.current_cap_kbps = None,
        }
        self.last_sent = Some(now);
    }
}

/// Per-connection shared handle: the RTCP feedback task and the
/// settings handler both lock `state` across their whole
/// decide → send → commit sequence (see the module-level concurrency
/// contract).
#[derive(Debug)]
pub struct AdaptiveBitrateShared {
    pub state: Mutex<AdaptiveBitrateState>,
}

impl AdaptiveBitrateShared {
    pub fn new(enabled: bool) -> Self {
        Self {
            state: Mutex::new(AdaptiveBitrateState::new(enabled)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    fn capped_state(cap: u32, sent_at: Instant) -> AdaptiveBitrateState {
        let mut s = AdaptiveBitrateState::new(true);
        s.commit(CapDirective::SetCap(cap), sent_at);
        s
    }

    #[test]
    fn disabled_state_never_emits() {
        let s = AdaptiveBitrateState::new(false);
        assert_eq!(s.decide_on_remb(t0(), 2_000_000.0), None);
    }

    #[test]
    fn garbage_remb_is_ignored() {
        let s = AdaptiveBitrateState::new(true);
        assert_eq!(s.decide_on_remb(t0(), 0.0), None);
        assert_eq!(s.decide_on_remb(t0(), -5.0), None);
        assert_eq!(s.decide_on_remb(t0(), f64::NAN), None);
    }

    #[test]
    fn unconstrained_link_stays_silent_when_uncapped() {
        let s = AdaptiveBitrateState::new(true);
        // 80 Mbps REMB → headroom target ≥ MAX_CAP → no directive.
        assert_eq!(s.decide_on_remb(t0(), 80_000_000.0), None);
    }

    #[test]
    fn constrained_link_engages_with_headroom() {
        let s = AdaptiveBitrateState::new(true);
        // 8 Mbps REMB → 8000 * 0.85 = 6800 kbps cap.
        assert_eq!(
            s.decide_on_remb(t0(), 8_000_000.0),
            Some(CapDirective::SetCap(6_800))
        );
    }

    #[test]
    fn floor_protects_against_decayed_estimates() {
        let s = AdaptiveBitrateState::new(true);
        // A decayed 200 kbps estimate must clamp to the floor, not
        // starve the encoder.
        assert_eq!(
            s.decide_on_remb(t0(), 200_000.0),
            Some(CapDirective::SetCap(MIN_CAP_KBPS))
        );
    }

    #[test]
    fn urgent_drop_bypasses_send_interval() {
        let now = t0();
        // Cap committed *just now* — interval not yet elapsed.
        let s = capped_state(10_000, now);
        // 4 Mbps REMB → target 3400, a 66% drop → urgent, sent despite
        // the interval.
        assert_eq!(
            s.decide_on_remb(now + Duration::from_millis(100), 4_000_000.0),
            Some(CapDirective::SetCap(3_400))
        );
    }

    #[test]
    fn small_jitter_is_suppressed_by_hysteresis() {
        let now = t0();
        let s = capped_state(10_000, now);
        // Target 9350 (-6.5%) → within hysteresis → silent, even after
        // the interval elapsed.
        assert_eq!(
            s.decide_on_remb(now + Duration::from_secs(2), 11_000_000.0),
            None
        );
    }

    #[test]
    fn moderate_drop_respects_send_interval() {
        let now = t0();
        let s = capped_state(10_000, now);
        // Target 6800 (-32%): not urgent → suppressed inside the
        // interval, emitted after it.
        assert_eq!(
            s.decide_on_remb(now + Duration::from_millis(300), 8_000_000.0),
            None
        );
        assert_eq!(
            s.decide_on_remb(now + Duration::from_secs(2), 8_000_000.0),
            Some(CapDirective::SetCap(6_800))
        );
    }

    #[test]
    fn recovery_is_rate_limited() {
        let now = t0();
        let s = capped_state(2_000, now);
        // REMB says 40 Mbps but one second of recovery only allows
        // +50% → 3000 kbps.
        assert_eq!(
            s.decide_on_remb(now + Duration::from_secs(1), 40_000_000.0),
            Some(CapDirective::SetCap(3_000))
        );
    }

    #[test]
    fn full_recovery_clears_the_cap() {
        let now = t0();
        let s = capped_state(40_000, now);
        // Allowed growth after 1 s: 60000 ≥ MAX_CAP and the estimate
        // agrees → the cap is removed rather than chased upwards.
        assert_eq!(
            s.decide_on_remb(now + Duration::from_secs(1), 80_000_000.0),
            Some(CapDirective::Clear)
        );
    }

    #[test]
    fn disable_with_active_cap_emits_clear_and_stops_decisions() {
        let now = t0();
        let mut s = capped_state(5_000, now);
        assert_eq!(
            s.set_enabled_and_decide_clear(false),
            Some(CapDirective::Clear)
        );
        // Enabled flag flipped immediately: REMB decisions stop even
        // though the Clear has not been committed yet.
        assert!(!s.enabled());
        assert_eq!(
            s.decide_on_remb(now + Duration::from_secs(5), 2_000_000.0),
            None
        );
    }

    #[test]
    fn disable_without_cap_is_silent() {
        let mut s = AdaptiveBitrateState::new(true);
        assert_eq!(s.set_enabled_and_decide_clear(false), None);
        // Re-disable / enable transitions are idempotent and silent.
        assert_eq!(s.set_enabled_and_decide_clear(false), None);
        assert_eq!(s.set_enabled_and_decide_clear(true), None);
    }

    #[test]
    fn uncommitted_decision_does_not_affect_hysteresis() {
        let now = t0();
        let s = capped_state(10_000, now);
        let later = now + Duration::from_secs(2);
        // First decision: target 6800. NOT committed (simulating a
        // failed IPC send).
        assert_eq!(
            s.decide_on_remb(later, 8_000_000.0),
            Some(CapDirective::SetCap(6_800))
        );
        // Re-deciding immediately must yield the same directive — the
        // uncommitted attempt left no trace, so the retry is not
        // suppressed as a "no change" by hysteresis.
        assert_eq!(
            s.decide_on_remb(later, 8_000_000.0),
            Some(CapDirective::SetCap(6_800))
        );
    }

    #[test]
    fn commit_after_clear_resets_to_uncapped_behaviour() {
        let now = t0();
        let mut s = capped_state(5_000, now);
        s.commit(CapDirective::Clear, now + Duration::from_secs(1));
        assert_eq!(s.current_cap_kbps(), None);
        // Uncapped again: an unconstrained estimate stays silent.
        assert_eq!(
            s.decide_on_remb(now + Duration::from_secs(3), 80_000_000.0),
            None
        );
    }

    #[test]
    fn wire_encoding_matches_ipc_sentinels() {
        assert_eq!(CapDirective::SetCap(4_200).wire_kbps(), 4_200);
        assert_eq!(CapDirective::Clear.wire_kbps(), 0);
    }
}
