//! The host security policy as something a copy of it can be reasoned about.
//!
//! The daemon owns the policy; a session worker holds a copy and decides
//! permission requests from it. Two questions have to be answerable on the copy
//! side, and they are not the same question:
//!
//! - *Is this policy I just received newer than the one I hold?* — a total
//!   order, answered by [`PolicySnapshot::seq`].
//! - *Which capabilities actually changed since I cached an approval?* — per
//!   capability, answered by the sequence number each one was last changed at.
//!
//! One counter cannot do both. Ordering by a per-capability counter is
//! impossible because they advance independently, and using the whole-policy
//! sequence to invalidate caches would discard an approval for whiteboard
//! because file transfer changed. So each capability records the `seq` at which
//! it last changed value: the ordering stays global, the invalidation stays
//! per capability, and the two cannot drift apart because there is only ever
//! one counter being written.

use serde::{Deserialize, Serialize};
use wincode::{SchemaRead, SchemaWrite};

use crate::model::security_settings::{SecurityPermissionType, SecuritySettings};

/// What one capability is set to, and the stamp that setting carries.
///
/// The two travel together so a gate cannot end up holding one from before a
/// policy change and the other from after it — see [`PolicySnapshot::capability`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityState {
    /// The configured value: `Some(true)` allow, `Some(false)` deny, `None` ask.
    pub permission: Option<bool>,
    /// The sequence at which this capability last changed value.
    pub generation: u64,
}

/// The sequence number at which each capability last changed value.
///
/// Named fields rather than an array indexed by capability: an array silently
/// misaligns when a dimension is added or reordered, and nothing about the
/// resulting mix-up would fail to compile.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead,
)]
pub struct PolicyGenerations {
    pub allow_remote_control: u64,
    pub allow_clipboard_sync: u64,
    pub allow_private_screen: u64,
    pub allow_whiteboard: u64,
    pub allow_terminal: u64,
    pub allow_file_browse: u64,
    pub allow_file_delete: u64,
    pub allow_file_transfer: u64,
}

impl PolicyGenerations {
    fn slot(&mut self, capability: SecurityPermissionType) -> &mut u64 {
        match capability {
            SecurityPermissionType::RemoteControl => &mut self.allow_remote_control,
            SecurityPermissionType::ClipboardSync => &mut self.allow_clipboard_sync,
            SecurityPermissionType::PrivateScreen => &mut self.allow_private_screen,
            SecurityPermissionType::Whiteboard => &mut self.allow_whiteboard,
            SecurityPermissionType::Terminal => &mut self.allow_terminal,
            SecurityPermissionType::FileBrowse => &mut self.allow_file_browse,
            SecurityPermissionType::FileDelete => &mut self.allow_file_delete,
            SecurityPermissionType::FileTransfer => &mut self.allow_file_transfer,
        }
    }

    /// The sequence number at which `capability` last changed value.
    pub fn of(&self, capability: SecurityPermissionType) -> u64 {
        match capability {
            SecurityPermissionType::RemoteControl => self.allow_remote_control,
            SecurityPermissionType::ClipboardSync => self.allow_clipboard_sync,
            SecurityPermissionType::PrivateScreen => self.allow_private_screen,
            SecurityPermissionType::Whiteboard => self.allow_whiteboard,
            SecurityPermissionType::Terminal => self.allow_terminal,
            SecurityPermissionType::FileBrowse => self.allow_file_browse,
            SecurityPermissionType::FileDelete => self.allow_file_delete,
            SecurityPermissionType::FileTransfer => self.allow_file_transfer,
        }
    }
}

/// A security policy with the bookkeeping needed to distribute it.
///
/// The policy is only reachable through [`PolicySnapshot::set`], which stamps
/// the changes as it makes them. Assigning a value and recording that it
/// changed are therefore the same operation, not two steps a caller could get
/// half-right — an unstamped change is the failure this type exists to prevent,
/// because it leaves a revoked capability in force behind a cached approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct PolicySnapshot {
    seq: u64,
    generations: PolicyGenerations,
    security: SecuritySettings,
}

impl PolicySnapshot {
    /// The initial policy, at sequence zero.
    pub fn new(security: SecuritySettings) -> Self {
        Self {
            seq: 0,
            generations: PolicyGenerations::default(),
            security,
        }
    }

    /// Where this policy sits in the total order. A copy compares this against
    /// its own to decide whether an arriving policy is worth adopting.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn generations(&self) -> &PolicyGenerations {
        &self.generations
    }

    pub fn security(&self) -> &SecuritySettings {
        &self.security
    }

    /// The sequence number at which `capability` last changed value. A cached
    /// decision taken at an older sequence than this no longer reflects the
    /// policy and has to be decided again.
    pub fn changed_at(&self, capability: SecurityPermissionType) -> u64 {
        self.generations.of(capability)
    }

    /// One capability's setting together with the stamp it carries.
    ///
    /// The pair has to come from the same policy. A gate that read the value,
    /// let the policy move, and then read the stamp would decide from the old
    /// setting and file the answer under the new one — caching an approval the
    /// operator has since revoked, and offering it to the upstream compare-and-
    /// set as though it had been taken under the revocation.
    pub fn capability(&self, capability: SecurityPermissionType) -> CapabilityState {
        CapabilityState {
            permission: capability.read(&self.security),
            generation: self.generations.of(capability),
        }
    }

    /// Adopt `security` as the policy.
    ///
    /// Advances the sequence and stamps every capability whose value actually
    /// changed. A policy that differs only in `approval_timeout` still advances
    /// the sequence — copies need to see it — but stamps no capability, so it
    /// does not throw away approvals the user has already given. An identical
    /// policy is not a change at all and advances nothing, which keeps a
    /// re-save of unchanged settings from invalidating anything.
    ///
    /// Returns whether anything changed.
    pub fn set(&mut self, security: SecuritySettings) -> bool {
        if self.security == security {
            return false;
        }
        let next_seq = self.seq + 1;
        for &capability in SecurityPermissionType::ALL {
            if capability.read(&self.security) != capability.read(&security) {
                *self.generations.slot(capability) = next_seq;
            }
        }
        self.seq = next_seq;
        self.security = security;
        true
    }

    /// Set one capability, as a remembered approval does.
    ///
    /// Returns whether the value changed.
    pub fn set_capability(
        &mut self,
        capability: SecurityPermissionType,
        value: Option<bool>,
    ) -> bool {
        let mut security = self.security.clone();
        capability.write(&mut security, value);
        self.set(security)
    }

    /// The capabilities whose value differs from `previous`, by comparing when
    /// each was last changed. A copy uses this to decide which cached approvals
    /// an arriving policy invalidates, without having to hold the old policy.
    pub fn changed_since(&self, previous: &PolicyGenerations) -> Vec<SecurityPermissionType> {
        SecurityPermissionType::ALL
            .iter()
            .copied()
            .filter(|&capability| self.generations.of(capability) != previous.of(capability))
            .collect()
    }

    /// Whether `self` claims to be at least as new as `other` while disagreeing
    /// with it about the policy.
    ///
    /// That combination cannot arise from ordinary sequencing: a newer policy
    /// carries a higher sequence. It means a change was published without being
    /// stamped, so the sequence no longer orders anything, and a copy that
    /// trusts it would keep serving approvals the operator believes are revoked.
    /// The caller's response is to fall back to the stricter of the two and ask
    /// to be resynchronized rather than pick a winner.
    pub fn contradicts(&self, other: &Self) -> bool {
        self.seq == other.seq
            && (self.security != other.security || self.generations != other.generations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(security: SecuritySettings) -> PolicySnapshot {
        PolicySnapshot::new(security)
    }

    fn with_terminal(value: Option<bool>) -> SecuritySettings {
        SecuritySettings {
            allow_terminal: value,
            ..SecuritySettings::default()
        }
    }

    /// The whole point of the stamps: revoking one capability must not discard
    /// an approval the user gave for another.
    #[test]
    fn only_the_capability_that_changed_is_stamped() {
        let mut snapshot = policy(SecuritySettings::default());
        let before = *snapshot.generations();

        assert!(snapshot.set(with_terminal(Some(false))));

        assert_eq!(
            snapshot.changed_since(&before),
            vec![SecurityPermissionType::Terminal]
        );
    }

    /// Every dimension gets the same treatment — testing one and generalizing
    /// is exactly how a missing dimension would go unnoticed.
    #[test]
    fn every_capability_stamps_itself_and_nothing_else() {
        for &capability in SecurityPermissionType::ALL {
            let mut snapshot = policy(SecuritySettings::default());
            let before = *snapshot.generations();

            assert!(
                snapshot.set_capability(capability, Some(false)),
                "{capability:?} should register as a change"
            );

            assert_eq!(
                snapshot.changed_since(&before),
                vec![capability],
                "{capability:?} stamped the wrong set"
            );
            assert_eq!(
                capability.read(snapshot.security()),
                Some(false),
                "{capability:?} did not take the new value"
            );
        }
    }

    /// A change to any capability has to move the total order too, or a copy
    /// would reject the policy carrying it as "not newer".
    #[test]
    fn a_changed_capability_always_advances_the_sequence() {
        for &capability in SecurityPermissionType::ALL {
            let mut snapshot = policy(SecuritySettings::default());
            let before = snapshot.seq();

            snapshot.set_capability(capability, Some(true));

            assert!(
                snapshot.seq() > before,
                "{capability:?} left the sequence behind"
            );
            assert_eq!(snapshot.changed_at(capability), snapshot.seq());
        }
    }

    /// The timeout is not a capability. Copies still need the new value, so the
    /// sequence advances, but stamping a capability here would revoke approvals
    /// the user already granted for an unrelated reason.
    #[test]
    fn changing_only_the_timeout_stamps_no_capability() {
        let mut snapshot = policy(SecuritySettings::default());
        let before_seq = snapshot.seq();
        let before = *snapshot.generations();

        assert!(snapshot.set(SecuritySettings {
            approval_timeout: Some(90),
            ..SecuritySettings::default()
        }));

        assert!(snapshot.seq() > before_seq);
        assert!(snapshot.changed_since(&before).is_empty());
    }

    /// Saving settings that did not change is common (the settings page writes
    /// the whole policy back). It must not look like a change to anyone.
    #[test]
    fn an_unchanged_policy_advances_nothing() {
        let mut snapshot = policy(with_terminal(Some(true)));
        let before_seq = snapshot.seq();
        let before = *snapshot.generations();

        assert!(!snapshot.set(with_terminal(Some(true))));

        assert_eq!(snapshot.seq(), before_seq);
        assert!(snapshot.changed_since(&before).is_empty());
    }

    /// Comparing values alone cannot see a revocation that was undone: the
    /// value at the end equals the value at the start. The stamp records that
    /// the capability moved, which is what a remembered approval taken before
    /// the round trip has to be checked against.
    #[test]
    fn a_value_that_returns_to_its_original_still_counts_as_changed() {
        let mut snapshot = policy(SecuritySettings::default());
        let before = *snapshot.generations();

        snapshot.set(with_terminal(Some(false)));
        snapshot.set(SecuritySettings::default());

        assert_eq!(
            capability_value(&snapshot, SecurityPermissionType::Terminal),
            None,
            "the value is back where it started"
        );
        assert_eq!(
            snapshot.changed_since(&before),
            vec![SecurityPermissionType::Terminal],
            "but the capability did move, and a decision from before is stale"
        );
    }

    fn capability_value(
        snapshot: &PolicySnapshot,
        capability: SecurityPermissionType,
    ) -> Option<bool> {
        capability.read(snapshot.security())
    }

    /// Two policies at the same sequence must be the same policy. Anything else
    /// means a change went out unstamped and the ordering is no longer sound.
    #[test]
    fn the_same_sequence_carrying_a_different_policy_is_a_contradiction() {
        let held = policy(SecuritySettings::default());
        let mut forged = policy(with_terminal(Some(true)));
        // Same sequence, different policy — what an unstamped publish looks like
        // from the receiving side.
        assert_eq!(forged.seq(), held.seq());

        assert!(forged.contradicts(&held));

        // An honest update disagrees about the policy but says so in the order.
        forged.set(with_terminal(Some(false)));
        assert!(!forged.contradicts(&held));
    }

    #[test]
    fn an_identical_policy_at_the_same_sequence_is_not_a_contradiction() {
        let held = policy(with_terminal(Some(true)));
        let same = policy(with_terminal(Some(true)));

        assert!(!same.contradicts(&held));
    }

    /// The counters describe a running distribution, not the configuration, and
    /// `Settings` persists every field it carries. Leaking them into the file
    /// would put bookkeeping into `config.toml` and make it meaningful across
    /// restarts, which it is not.
    #[test]
    fn only_the_policy_itself_is_configuration() {
        let mut snapshot = policy(SecuritySettings::default());
        snapshot.set(with_terminal(Some(false)));

        let serialized = toml::to_string(snapshot.security()).expect("serialize");

        assert!(!serialized.contains("seq"), "got:\n{serialized}");
        assert!(!serialized.contains("generation"), "got:\n{serialized}");
    }
}
