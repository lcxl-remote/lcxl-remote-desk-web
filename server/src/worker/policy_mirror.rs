//! The worker's copy of the host security policy.
//!
//! A session worker decides permission requests but does not own the policy —
//! the daemon does. The mirror is what the worker decides from, and it is
//! deliberately reachable without `await`: it is updated on the transport
//! reader task, ahead of the event main loop, so a policy change lands even
//! while the main loop is parked on an approval prompt that the change is
//! precisely what would resolve.
//!
//! Arriving policies are ordered by their own sequence rather than by delivery
//! order, and a policy that cannot be reconciled with the one held is resolved
//! by taking the stricter reading of the two and asking to be resynchronized.
//! The mirror never resolves a disagreement in the permissive direction.

use std::sync::RwLock;

use desk_ipc_protocol::message::PolicyApplyOutcome;
use desk_signal_facade::model::policy_snapshot::PolicySnapshot;
use desk_signal_facade::model::security_settings::{SecurityPermissionType, SecuritySettings};

use crate::model::security_approval::meet_permission;

#[derive(Debug)]
struct MirrorState {
    held: PolicySnapshot,
    /// Set when the mirror could not reconcile an arriving policy with the one
    /// it held. While set, the mirror is knowingly holding something the daemon
    /// never published, so the next policy to arrive is adopted whatever the
    /// sequences say — otherwise the locally tightened policy, whose sequence
    /// has moved on, would reject the very republication that would fix it.
    awaiting_resync: bool,
}

#[derive(Debug)]
pub struct PolicyMirror {
    state: RwLock<MirrorState>,
}

impl PolicyMirror {
    pub fn new(initial: PolicySnapshot) -> Self {
        Self {
            state: RwLock::new(MirrorState {
                held: initial,
                awaiting_resync: false,
            }),
        }
    }

    /// The policy currently held.
    pub fn snapshot(&self) -> PolicySnapshot {
        self.state.read().expect("policy mirror").held.clone()
    }

    /// The configured value of one capability.
    pub fn permission(&self, capability: SecurityPermissionType) -> Option<bool> {
        capability.read(self.state.read().expect("policy mirror").held.security())
    }

    /// When the capability last changed. A decision cached at an earlier
    /// sequence predates the current policy and has to be taken again.
    pub fn changed_at(&self, capability: SecurityPermissionType) -> u64 {
        self.state
            .read()
            .expect("policy mirror")
            .held
            .changed_at(capability)
    }

    pub fn security(&self) -> SecuritySettings {
        self.state
            .read()
            .expect("policy mirror")
            .held
            .security()
            .clone()
    }

    /// Take an arriving policy into the mirror and report what happened.
    pub fn apply(&self, arriving: PolicySnapshot) -> PolicyApplyOutcome {
        let mut state = self.state.write().expect("policy mirror");

        if state.awaiting_resync {
            state.held = arriving;
            state.awaiting_resync = false;
            return applied(&state.held);
        }

        if arriving.seq() < state.held.seq() {
            // Already superseded; the daemon is behind its own publication, not
            // the mirror. Answering with what is held keeps the daemon's view
            // of this worker accurate.
            return applied(&state.held);
        }

        if arriving.contradicts(&state.held) {
            // Same sequence, different policy: something changed without being
            // stamped, so the sequence no longer orders these two and there is
            // no sound basis for preferring either. Hold the stricter reading
            // until the daemon republishes.
            let stricter = stricter_of(state.held.security(), arriving.security());
            state.held.set(stricter);
            state.awaiting_resync = true;
            log::error!(
                "[security] policy at sequence {} disagrees with the one held; \
                 holding the stricter reading and asking for a resync",
                arriving.seq()
            );
            return PolicyApplyOutcome::NeedsResync {
                seq: state.held.seq(),
            };
        }

        state.held = arriving;
        applied(&state.held)
    }
}

fn applied(held: &PolicySnapshot) -> PolicyApplyOutcome {
    PolicyApplyOutcome::Applied {
        seq: held.seq(),
        generations: *held.generations(),
    }
}

/// The more restrictive reading of two policies, capability by capability.
///
/// The approval timeout is taken from the arriving policy rather than met: it
/// is not a capability, and "stricter" is not well defined for it once "never"
/// is spelled as zero.
fn stricter_of(held: &SecuritySettings, arriving: &SecuritySettings) -> SecuritySettings {
    let mut stricter = arriving.clone();
    for &capability in SecurityPermissionType::ALL {
        capability.write(
            &mut stricter,
            meet_permission(capability.read(held), capability.read(arriving)),
        );
    }
    stricter
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(security: SecuritySettings) -> PolicySnapshot {
        PolicySnapshot::new(security)
    }

    fn terminal(value: Option<bool>) -> SecuritySettings {
        SecuritySettings {
            allow_terminal: value,
            ..SecuritySettings::default()
        }
    }

    fn seq_of(outcome: &PolicyApplyOutcome) -> u64 {
        match outcome {
            PolicyApplyOutcome::Applied { seq, .. } => *seq,
            PolicyApplyOutcome::NeedsResync { seq } => *seq,
        }
    }

    #[test]
    fn a_newer_policy_is_adopted() {
        let mirror = PolicyMirror::new(policy(SecuritySettings::default()));
        let mut next = policy(SecuritySettings::default());
        next.set(terminal(Some(false)));

        let outcome = mirror.apply(next.clone());

        assert!(matches!(outcome, PolicyApplyOutcome::Applied { .. }));
        assert_eq!(
            mirror.permission(SecurityPermissionType::Terminal),
            Some(false)
        );
        assert_eq!(seq_of(&outcome), next.seq());
    }

    /// Delivery order is not authority. A policy that lost a race must not undo
    /// the newer one that already landed.
    #[test]
    fn an_older_policy_does_not_displace_a_newer_one() {
        let older = policy(SecuritySettings::default());
        let mut newer = older.clone();
        newer.set(terminal(Some(false)));
        let mirror = PolicyMirror::new(newer.clone());

        let outcome = mirror.apply(older);

        assert_eq!(
            mirror.permission(SecurityPermissionType::Terminal),
            Some(false),
            "the newer policy must survive"
        );
        assert_eq!(
            seq_of(&outcome),
            newer.seq(),
            "the answer reports what is actually held"
        );
    }

    /// Two policies at the same sequence that disagree mean a change went out
    /// unstamped. Picking the arriving one would silently apply an unrevoked
    /// capability; the mirror keeps the stricter reading instead.
    #[test]
    fn an_unstamped_change_is_resolved_towards_denial() {
        let mirror = PolicyMirror::new(policy(terminal(Some(false))));
        // Same sequence as what is held, but permissive — an unstamped publish.
        let forged = policy(terminal(Some(true)));

        let outcome = mirror.apply(forged);

        assert!(matches!(outcome, PolicyApplyOutcome::NeedsResync { .. }));
        assert_eq!(
            mirror.permission(SecurityPermissionType::Terminal),
            Some(false),
            "the deny has to survive a contradiction"
        );
    }

    /// The tightening moves the mirror's own sequence past what the daemon
    /// published, so the republication that resolves it would otherwise look
    /// stale and be discarded — leaving the worker permanently degraded.
    #[test]
    fn a_republished_policy_is_taken_after_a_contradiction() {
        let mirror = PolicyMirror::new(policy(terminal(Some(false))));
        mirror.apply(policy(terminal(Some(true))));

        // The daemon republishes what it actually holds, at its own sequence,
        // which by now is behind the mirror's locally tightened one.
        let authoritative = policy(terminal(Some(true)));
        let outcome = mirror.apply(authoritative);

        assert!(matches!(outcome, PolicyApplyOutcome::Applied { .. }));
        assert_eq!(
            mirror.permission(SecurityPermissionType::Terminal),
            Some(true),
            "the daemon is authoritative once it has spoken again"
        );
    }

    /// Only the first policy after a contradiction is taken unconditionally.
    #[test]
    fn ordering_resumes_after_a_resync() {
        let mirror = PolicyMirror::new(policy(terminal(Some(false))));
        mirror.apply(policy(terminal(Some(true))));
        mirror.apply(policy(terminal(Some(true))));

        let mut newer = policy(terminal(Some(true)));
        newer.set(terminal(Some(false)));
        mirror.apply(newer);
        // An older policy is once again just an older policy.
        mirror.apply(policy(terminal(Some(true))));

        assert_eq!(
            mirror.permission(SecurityPermissionType::Terminal),
            Some(false)
        );
    }

    /// Republishing the identical policy is not a disagreement — it happens
    /// whenever a worker restarts and the daemon replays what it holds.
    #[test]
    fn republishing_the_same_policy_is_accepted_quietly() {
        let held = policy(terminal(Some(true)));
        let mirror = PolicyMirror::new(held.clone());

        let outcome = mirror.apply(held);

        assert!(matches!(outcome, PolicyApplyOutcome::Applied { .. }));
        assert_eq!(
            mirror.permission(SecurityPermissionType::Terminal),
            Some(true)
        );
    }

    /// A contradiction on one capability must not drag the others down with it.
    #[test]
    fn tightening_leaves_agreeing_capabilities_alone() {
        let held = policy(SecuritySettings {
            allow_terminal: Some(false),
            allow_file_browse: Some(true),
            ..SecuritySettings::default()
        });
        let mirror = PolicyMirror::new(held);

        mirror.apply(policy(SecuritySettings {
            allow_terminal: Some(true),
            allow_file_browse: Some(true),
            ..SecuritySettings::default()
        }));

        assert_eq!(
            mirror.permission(SecurityPermissionType::Terminal),
            Some(false)
        );
        assert_eq!(
            mirror.permission(SecurityPermissionType::FileBrowse),
            Some(true),
            "an agreeing capability keeps its value"
        );
    }
}
