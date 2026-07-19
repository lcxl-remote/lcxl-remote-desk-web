//! Execution lifecycle frames shared by every path that runs a command.
//!
//! A command used to be a request and a result with nothing in between, so an
//! upstream that heard nothing could not tell a long-running command from a lost
//! one and had to guess from a clock. These frames remove the guess: the host
//! says when it accepted an execution, that it is still running, and — on demand
//! — what it currently believes about any generation it was ever told about.
//!
//! The types are defined once and carried by all three envelopes (the agentic
//! edge, the fleet edge, and the relayed control-end channel) so the three cannot
//! drift into three dialects of the same conversation.
//!
//! # No sequence numbers
//!
//! Frames carry no counter. Ordering between *states* is already total enough to
//! fence a late frame: an execution moves reserved → running → terminal and never
//! back, so [`ExecState::may_supersede`] decides whether an arriving frame is
//! news without anything having to be counted. A counter would need to survive a
//! host restart to be safe, and a counter that resets discards legitimate frames
//! — a failure mode introduced solely to solve a problem the state order already
//! solves.
//!
//! Heartbeats are the one genuinely repeatable event, and they carry the host's
//! own elapsed time rather than a sequence: two heartbeats out of order are
//! indistinguishable from one lost heartbeat, and both are harmless.
//!
//! # Cancel replies with state
//!
//! There is no dedicated cancel acknowledgement. Cancelling asks the host to
//! reclaim a generation, and "what is that generation's state now" is exactly
//! what [`ExecStateReply`] answers — so a cancel and a state query share one
//! reply type, and an upstream implements one rule (keep asking until the state
//! is terminal) instead of two that can disagree.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Host → upstream: something happened to an execution that is not its result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ExecLifecycleEvent {
    /// The host has durably reserved the execution and started the command.
    /// Reported once, and the upstream counterpart of the host's running state.
    Accepted {
        /// How the host can reclaim this execution's process tree, when it can
        /// name it yet. Absent on platforms that cannot name a container until
        /// the child exists and the host crashed inside that window.
        containment_identity: Option<String>,
    },
    /// The command is still running. Carries the host's own elapsed time rather
    /// than a wall clock, so an upstream never has to trust the host's clock or
    /// reconcile two clocks to decide whether progress is being made.
    Heartbeat {
        /// Milliseconds since the host reserved this execution.
        running_ms: u64,
    },
}

/// Host → upstream lifecycle frame, fenced by the generation it describes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ExecLifecyclePayload {
    /// The one dispatch this frame is about. A frame whose generation does not
    /// match what the receiver is tracking is dropped, never applied to another.
    pub execution_generation: String,
    pub event: ExecLifecycleEvent,
}

/// Upstream → host: act on an execution already in flight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ExecControlAction {
    /// Reclaim this execution's process tree. Always answerable: the host
    /// contains every execution in a process group or job object, so stopping
    /// one does not depend on the command cooperating.
    Cancel {
        /// Who asked, for the audit record. The host does not authorise on this
        /// — the upstream does — but a cancel with no attributable requester is
        /// not a record anyone can act on later.
        requested_by: String,
    },
    /// Report what the host believes about this generation, changing nothing.
    QueryState,
}

/// Upstream → host control frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ExecControlPayload {
    pub execution_generation: String,
    pub action: ExecControlAction,
}

/// What the host's ledger says about one generation.
///
/// Mirrors the host-side ledger states, plus [`ExecState::Unknown`] for a
/// generation the host has no record of at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecState {
    /// Durably reserved; the command has not been reported as started.
    Reserved,
    /// Running.
    Running,
    /// Finished, with a result the host can hand back.
    Terminal,
    /// The host cannot say whether it ran — it lost track of it across a crash.
    /// Not a failure: a mutating command in this state needs a human, not a retry.
    Indeterminate,
    /// The command was never started because the spawn itself failed.
    SpawnFailed,
    /// The host has no record of this generation.
    Unknown,
}

impl ExecState {
    /// Whether this state settles the execution, so an upstream can stop asking.
    ///
    /// [`ExecState::Unknown`] is deliberately not settled: a host with no record
    /// has not answered the question, and treating silence as an answer is the
    /// guessing this whole mechanism exists to remove.
    pub fn is_settled(self) -> bool {
        matches!(
            self,
            ExecState::Terminal | ExecState::Indeterminate | ExecState::SpawnFailed
        )
    }

    /// Position in the lifecycle order, used to fence late frames.
    fn rank(self) -> Option<u8> {
        match self {
            ExecState::Reserved => Some(0),
            ExecState::Running => Some(1),
            ExecState::Terminal | ExecState::Indeterminate | ExecState::SpawnFailed => Some(2),
            // Not a position in the order — it is the absence of one.
            ExecState::Unknown => None,
        }
    }

    /// Whether a receiver holding `previous` should adopt `self`.
    ///
    /// This is what makes sequence numbers unnecessary: an execution only ever
    /// moves forward, so a frame is news exactly when it reports a later state
    /// than the one already recorded. A duplicate or reordered frame reports the
    /// same or an earlier state and is ignored.
    ///
    /// [`ExecState::Unknown`] never supersedes anything. A host that lost its
    /// ledger — or was asked about a generation it never saw — must not be able
    /// to erase a terminal result the upstream already holds.
    pub fn may_supersede(self, previous: Option<ExecState>) -> bool {
        let Some(new_rank) = self.rank() else {
            return false;
        };
        match previous.and_then(ExecState::rank) {
            Some(old_rank) => new_rank > old_rank,
            // Nothing recorded yet, or only an Unknown: any real state is news.
            None => true,
        }
    }
}

/// Host → upstream: the answer to both a cancel and a state query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ExecStateReplyPayload {
    pub execution_generation: String,
    pub state: ExecState,
    /// How the host would reclaim this execution, when it knows.
    pub containment_identity: Option<String>,
    /// Milliseconds since reservation, while the execution is still in flight.
    pub running_ms: Option<u64>,
    /// Model-safe elaboration — why a state is indeterminate, or why a spawn
    /// failed. Never the command's output, which travels on the result path.
    pub detail: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The order of states alone decides whether an arriving frame is news, which
    /// is why no frame carries a sequence number.
    #[test]
    fn a_frame_is_news_only_when_it_reports_a_later_state() {
        assert!(ExecState::Reserved.may_supersede(None));
        assert!(ExecState::Running.may_supersede(Some(ExecState::Reserved)));
        assert!(ExecState::Terminal.may_supersede(Some(ExecState::Running)));

        // A duplicate is not news.
        assert!(!ExecState::Running.may_supersede(Some(ExecState::Running)));
        // Nor is a frame that arrived late and reports the past.
        assert!(!ExecState::Reserved.may_supersede(Some(ExecState::Running)));
        assert!(!ExecState::Running.may_supersede(Some(ExecState::Terminal)));
    }

    /// The three terminal states are peers: none of them can overwrite another,
    /// so a replayed terminal frame cannot turn a recorded result into a doubt.
    #[test]
    fn one_terminal_state_never_replaces_another() {
        for settled in [
            ExecState::Terminal,
            ExecState::Indeterminate,
            ExecState::SpawnFailed,
        ] {
            assert!(settled.is_settled());
            for other in [
                ExecState::Terminal,
                ExecState::Indeterminate,
                ExecState::SpawnFailed,
            ] {
                assert!(
                    !settled.may_supersede(Some(other)),
                    "{settled:?} overwrote {other:?}"
                );
            }
        }
    }

    /// A host that lost its ledger reports Unknown, and that must never erase what
    /// the upstream already knows — nor count as an answer that stops the asking.
    #[test]
    fn an_unknown_generation_erases_nothing_and_settles_nothing() {
        assert!(!ExecState::Unknown.is_settled());
        for previous in [
            ExecState::Reserved,
            ExecState::Running,
            ExecState::Terminal,
            ExecState::Indeterminate,
            ExecState::SpawnFailed,
        ] {
            assert!(
                !ExecState::Unknown.may_supersede(Some(previous)),
                "Unknown overwrote {previous:?}"
            );
        }
        // And a real state still lands over a previously unknown one.
        assert!(ExecState::Terminal.may_supersede(Some(ExecState::Unknown)));
    }

    #[test]
    fn lifecycle_frames_round_trip_with_snake_case_tags() {
        let frames = [
            ExecLifecyclePayload {
                execution_generation: "gen-1".into(),
                event: ExecLifecycleEvent::Accepted {
                    containment_identity: Some("pgid:4242".into()),
                },
            },
            ExecLifecyclePayload {
                execution_generation: "gen-1".into(),
                event: ExecLifecycleEvent::Heartbeat { running_ms: 1_500 },
            },
        ];
        for frame in frames {
            let json = serde_json::to_string(&frame).expect("encode");
            let back: ExecLifecyclePayload = serde_json::from_str(&json).expect("decode");
            assert_eq!(frame, back);
        }
        let json = serde_json::to_string(&ExecLifecycleEvent::Heartbeat { running_ms: 1 }).unwrap();
        assert!(json.contains("\"event\":\"heartbeat\""), "{json}");
    }

    #[test]
    fn control_frames_round_trip_with_snake_case_tags() {
        let frames = [
            ExecControlPayload {
                execution_generation: "gen-1".into(),
                action: ExecControlAction::Cancel {
                    requested_by: "operator:7".into(),
                },
            },
            ExecControlPayload {
                execution_generation: "gen-1".into(),
                action: ExecControlAction::QueryState,
            },
        ];
        for frame in frames {
            let json = serde_json::to_string(&frame).expect("encode");
            let back: ExecControlPayload = serde_json::from_str(&json).expect("decode");
            assert_eq!(frame, back);
        }
        let json = serde_json::to_string(&ExecControlAction::QueryState).unwrap();
        assert!(json.contains("\"action\":\"query_state\""), "{json}");
    }

    #[test]
    fn a_state_reply_round_trips_every_state() {
        for state in [
            ExecState::Reserved,
            ExecState::Running,
            ExecState::Terminal,
            ExecState::Indeterminate,
            ExecState::SpawnFailed,
            ExecState::Unknown,
        ] {
            let reply = ExecStateReplyPayload {
                execution_generation: "gen-1".into(),
                state,
                containment_identity: Some("pgid:1".into()),
                running_ms: Some(10),
                detail: None,
            };
            let json = serde_json::to_string(&reply).expect("encode");
            let back: ExecStateReplyPayload = serde_json::from_str(&json).expect("decode");
            assert_eq!(reply, back);
        }
    }
}
