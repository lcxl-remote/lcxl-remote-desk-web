//! Permission facts from accepted business decisions, without grant authority.

use super::*;
use crate::dynamic_run::{
    PermissionDecidedEvent, PermissionDecisionActor, PermissionDecisionSource,
    PermissionRequestState,
};

pub fn state_outcome(
    state: PermissionRequestState,
    actor: Option<PermissionDecisionActor>,
) -> PermissionOutcome {
    match state {
        PermissionRequestState::Pending | PermissionRequestState::NeedsRevalidation => {
            PermissionOutcome::Waiting
        }
        PermissionRequestState::Approved => PermissionOutcome::Approved,
        PermissionRequestState::PartiallyApproved => PermissionOutcome::Narrowed,
        PermissionRequestState::Denied => match actor {
            Some(PermissionDecisionActor::Owner) => PermissionOutcome::Denied,
            Some(PermissionDecisionActor::AiApproval) => PermissionOutcome::PolicyRejected,
            Some(PermissionDecisionActor::System) | None => PermissionOutcome::Unavailable,
        },
        PermissionRequestState::Replaced => PermissionOutcome::Revoked,
        PermissionRequestState::Withdrawn => PermissionOutcome::Cancelled,
    }
}

pub fn decision_outcome(event: &PermissionDecidedEvent) -> PermissionOutcome {
    let actor = match &event.decision_source {
        PermissionDecisionSource::UserDecision => PermissionDecisionActor::Owner,
        PermissionDecisionSource::AiApproval { .. } => PermissionDecisionActor::AiApproval,
        PermissionDecisionSource::ReviewUnavailable { .. } => PermissionDecisionActor::System,
    };
    state_outcome(event.resulting_state, Some(actor))
}

/// Call only after the original decision transaction commits. Missing original
/// observation state cannot change a decision, grant or permission resume.
pub fn decided(
    event: &PermissionDecidedEvent,
    occurred_at_ms: i64,
    mut submit: impl FnMut(ObservationEvent),
) {
    let Some(alias) = ObservationAlias::permission_request(&event.event.run_id, &event.request_id)
    else {
        return;
    };
    let outcome = decision_outcome(event);
    submit_outcome(alias, outcome, 0, occurred_at_ms, &mut submit);
}

fn submit_outcome(
    alias: ObservationAlias,
    outcome: PermissionOutcome,
    sequence: u32,
    occurred_at_ms: i64,
    mut submit: impl FnMut(ObservationEvent),
) {
    let stage = match outcome {
        PermissionOutcome::Approved | PermissionOutcome::Narrowed => StageOutcome::Passed,
        PermissionOutcome::Denied
        | PermissionOutcome::PolicyRejected
        | PermissionOutcome::Revoked => StageOutcome::Failed,
        PermissionOutcome::Waiting => StageOutcome::Attempted,
        PermissionOutcome::Unavailable
        | PermissionOutcome::Expired
        | PermissionOutcome::Cancelled
        | PermissionOutcome::NotReached => StageOutcome::NotReached,
    };
    let observation = ObservationEvent::deferred_tool(
        alias,
        ObservationPhase::Completed,
        sequence,
        occurred_at_ms,
        BTreeMap::from([(Stage::Permission, stage)]),
        outcome,
        None,
        InputIssue::None,
    );
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| submit(observation)));
}

/// These states come from the committed goal request, not a model answer or an
/// owner stop notification. Withdrawn currently means a superseding owner input.
pub fn goal_open_closed(
    request: &crate::goal::GoalOpenRequest,
    occurred_at_ms: i64,
    mut submit: impl FnMut(ObservationEvent),
) {
    use crate::goal::GoalOpenRequestState;
    let outcome = match request.state {
        GoalOpenRequestState::Pending => return,
        GoalOpenRequestState::Approved => PermissionOutcome::Approved,
        GoalOpenRequestState::Denied => PermissionOutcome::Denied,
        GoalOpenRequestState::Expired => PermissionOutcome::Expired,
        GoalOpenRequestState::Withdrawn => PermissionOutcome::Revoked,
    };
    let Some(alias) =
        ObservationAlias::goal_open_request(&request.conversation_id, &request.request_id)
    else {
        return;
    };
    submit_outcome(alias, outcome, 0, occurred_at_ms, &mut submit);
}

/// A directory decision retains only the original model-created request alias.
/// Owner-created selections have no model input and must not create fake gaps.
#[derive(Debug, Default)]
pub struct PendingDirectoryUpdate {
    alias: Option<ObservationAlias>,
}

impl PendingDirectoryUpdate {
    pub fn capture(
        session: &crate::session::PersistedAgentSession,
        update: &crate::file_scope::transaction::FileScopeUpdate,
    ) -> Self {
        use crate::file_scope::{DirectoryConsentSource, transaction::FileScopeMutation};
        let request_id = match &update.mutation {
            FileScopeMutation::Decide {
                directory_request_id,
                ..
            }
            | FileScopeMutation::Revoke {
                directory_request_id,
            } => directory_request_id,
            FileScopeMutation::Propose { .. } | FileScopeMutation::Select { .. } => {
                return Self::default();
            }
        };
        if session.file_scope.records().len() > crate::file_scope::MAX_SESSION_DIRECTORY_RECORDS {
            return Self::default();
        }
        let model_created = session.file_scope.records().iter().any(|record| {
            record.proposal.request_id == *request_id
                && matches!(
                    record.proposal.source,
                    DirectoryConsentSource::ModelProposal | DirectoryConsentSource::TaskContract
                )
        });
        Self {
            alias: model_created
                .then(|| ObservationAlias::directory_request(&session.conversation_id, request_id))
                .flatten(),
        }
    }

    /// Publish from the accepted receipt only after the enclosing commit. An
    /// idempotent no-change decision does not create another lifecycle fact.
    pub fn submit(
        self,
        receipt: &crate::file_scope::transaction::FileScopeReceipt,
        occurred_at_ms: i64,
        mut submit: impl FnMut(ObservationEvent),
    ) {
        use crate::file_scope::transaction::FileScopeMutation;
        if !receipt.changed {
            return;
        }
        let Some(alias) = self.alias else {
            return;
        };
        let (outcome, sequence) = match &receipt.update.mutation {
            FileScopeMutation::Decide { approve: true, .. } => (PermissionOutcome::Approved, 0),
            FileScopeMutation::Decide { approve: false, .. } => (PermissionOutcome::Denied, 0),
            FileScopeMutation::Revoke { .. } => (PermissionOutcome::Revoked, 1),
            FileScopeMutation::Propose { .. } | FileScopeMutation::Select { .. } => return,
        };
        submit_outcome(alias, outcome, sequence, occurred_at_ms, &mut submit);
    }
}

/// Capture only current unresolved aliases while a business stop transaction
/// already holds the session. Submit their cancellation after that commit.
pub fn waiting_aliases(session: &crate::session::PersistedAgentSession) -> Vec<ObservationAlias> {
    if session.permission_requests.len() > crate::dynamic_run::MAX_PERMISSION_REQUESTS {
        return Vec::new();
    }
    session
        .permission_requests
        .iter()
        .filter(|request| {
            request.input_revision == session.input_revision
                && request.state == PermissionRequestState::Pending
        })
        .filter_map(|request| {
            ObservationAlias::permission_request(&session.conversation_id, &request.request_id)
        })
        .collect()
}

/// Transient facts captured from an already loaded business session. Dropping
/// these before commit has no side effect; they never enter a durable receipt.
#[derive(Debug, Default)]
pub struct PendingEnds {
    updates: Vec<(ObservationAlias, PermissionOutcome)>,
}

impl PendingEnds {
    pub fn waiting(
        session: &crate::session::PersistedAgentSession,
        outcome: PermissionOutcome,
    ) -> Self {
        if !matches!(
            outcome,
            PermissionOutcome::Cancelled
                | PermissionOutcome::Revoked
                | PermissionOutcome::Expired
                | PermissionOutcome::Unavailable
        ) {
            return Self::default();
        }
        Self {
            updates: waiting_aliases(session)
                .into_iter()
                .map(|alias| (alias, outcome))
                .collect(),
        }
    }

    /// Use a task state already accepted by its original control transaction.
    /// Pausing or changing a lease does not end a permission request. A failed
    /// task has no approval verdict. Expiry is supplied only by the business
    /// branch that actually settled a deadline, never by a later observation clock.
    pub fn task_control(
        session: &crate::session::PersistedAgentSession,
        run: &crate::subagent::state::SubAgentRun,
        deadline_reached: bool,
    ) -> Self {
        use crate::subagent::SubAgentState;
        let outcome = match run.state {
            SubAgentState::Cancelled | SubAgentState::Cancelling => PermissionOutcome::Cancelled,
            SubAgentState::Failed if deadline_reached => PermissionOutcome::Expired,
            SubAgentState::Failed => PermissionOutcome::Unavailable,
            _ if session.input_revision != run.binding.input_revision => PermissionOutcome::Revoked,
            _ => return Self::default(),
        };
        Self::waiting(session, outcome)
    }

    pub fn extend(&mut self, other: Self) {
        let limit = (crate::subagent::SUBAGENT_ROOT_CAPACITY + 1)
            * crate::dynamic_run::MAX_PERMISSION_REQUESTS;
        let remaining = limit.saturating_sub(self.updates.len());
        self.updates
            .extend(other.updates.into_iter().take(remaining));
    }

    /// The transaction owner calls this only after the business commit succeeds.
    pub fn submit(self, occurred_at_ms: i64, mut submit: impl FnMut(ObservationEvent)) {
        for (alias, outcome) in self.updates {
            ended(alias, outcome, occurred_at_ms, &mut submit);
        }
    }
}

/// Only an accepted input revision invalidates the older undecided requests.
/// Immutable decisions and already granted authority retain their own facts.
pub fn input_superseded(
    session: &crate::session::PersistedAgentSession,
    occurred_at_ms: i64,
    mut submit: impl FnMut(ObservationEvent),
) {
    if session.permission_requests.len() > crate::dynamic_run::MAX_PERMISSION_REQUESTS {
        return;
    }
    for request in &session.permission_requests {
        if request.input_revision < session.input_revision
            && request.state == PermissionRequestState::NeedsRevalidation
            && let Some(alias) =
                ObservationAlias::permission_request(&session.conversation_id, &request.request_id)
        {
            ended(
                alias,
                PermissionOutcome::Revoked,
                occurred_at_ms,
                &mut submit,
            );
        }
    }
}

pub fn ended(
    alias: ObservationAlias,
    outcome: PermissionOutcome,
    occurred_at_ms: i64,
    mut submit: impl FnMut(ObservationEvent),
) {
    let stage = match outcome {
        PermissionOutcome::Revoked => StageOutcome::Failed,
        PermissionOutcome::Cancelled
        | PermissionOutcome::Expired
        | PermissionOutcome::Unavailable => StageOutcome::NotReached,
        _ => return,
    };
    let observation = ObservationEvent::deferred_tool(
        alias,
        ObservationPhase::Completed,
        1,
        occurred_at_ms,
        BTreeMap::from([(Stage::Permission, stage)]),
        outcome,
        None,
        InputIssue::None,
    );
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| submit(observation)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dynamic_run::{
        AGENT_RUN_EVENT_SCHEMA_VERSION, AgentRunEvent, AgentRunEventKind, PermissionDecisionItem,
        PermissionItemDecision,
    };

    fn decision(source: PermissionDecisionSource) -> PermissionDecidedEvent {
        PermissionDecidedEvent {
            event: AgentRunEvent {
                schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
                event_id: "decision-1".into(),
                run_id: "server-run".into(),
                event_seq: 2,
                input_revision: 1,
                kind: AgentRunEventKind::PermissionDecided,
                correlation_id: Some("server-request".into()),
                source_envelope_ids: vec![],
                result_envelope_ids: vec![],
                created_at: "2026-10-08T00:00:00Z".into(),
            },
            request_id: "server-request".into(),
            request_input_revision: 1,
            resulting_state: PermissionRequestState::Denied,
            items: vec![PermissionDecisionItem {
                item_id: "private-provider-call-id".into(),
                decision: PermissionItemDecision::Deny,
            }],
            decision_source: source,
        }
    }

    #[test]
    fn actual_decision_source_distinguishes_owner_policy_and_unavailable_verdicts() {
        for (actor, expected) in [
            (
                Some(PermissionDecisionActor::Owner),
                PermissionOutcome::Denied,
            ),
            (
                Some(PermissionDecisionActor::AiApproval),
                PermissionOutcome::PolicyRejected,
            ),
            (
                Some(PermissionDecisionActor::System),
                PermissionOutcome::Unavailable,
            ),
            (None, PermissionOutcome::Unavailable),
        ] {
            assert_eq!(
                state_outcome(PermissionRequestState::Denied, actor),
                expected
            );
            assert_eq!(
                state_outcome(PermissionRequestState::PartiallyApproved, actor),
                PermissionOutcome::Narrowed
            );
        }
        for (state, expected) in [
            (PermissionRequestState::Pending, PermissionOutcome::Waiting),
            (
                PermissionRequestState::NeedsRevalidation,
                PermissionOutcome::Waiting,
            ),
            (
                PermissionRequestState::Approved,
                PermissionOutcome::Approved,
            ),
            (PermissionRequestState::Replaced, PermissionOutcome::Revoked),
            (
                PermissionRequestState::Withdrawn,
                PermissionOutcome::Cancelled,
            ),
        ] {
            assert_eq!(state_outcome(state, None), expected);
        }
    }

    #[test]
    fn committed_decision_projection_has_no_content_no_execution_and_is_replay_stable() {
        let owner = decision(PermissionDecisionSource::UserDecision);
        let fault = decision(PermissionDecisionSource::ReviewUnavailable {
            reason_code: "approval_ai_fault".into(),
            reason: "private reviewer failure details".into(),
        });
        for (event, expected) in [
            (&owner, PermissionOutcome::Denied),
            (&fault, PermissionOutcome::Unavailable),
        ] {
            let before = serde_json::to_string(event).unwrap();
            let mut observations = Vec::new();
            for _ in 0..2 {
                decided(event, 2_000, |observation| observations.push(observation));
            }
            assert_eq!(observations.len(), 2);
            assert_eq!(observations[0], observations[1]);
            assert!(observations.iter().all(ObservationEvent::is_bounded));
            let observed = &observations[0];
            assert_eq!(
                observed.relation,
                Some(ObservationRelation::ResolveTool(
                    ObservationAlias::permission_request("server-run", "server-request").unwrap()
                ))
            );
            let ObservationPayload::Tool(tool) = &observed.payload else {
                panic!("tool fact required")
            };
            assert_eq!(tool.permission, expected);
            assert_eq!(tool.conclusion, InputConclusion::Unknown);
            assert_eq!(tool.issue, InputIssue::None);
            assert_eq!(tool.stages.len(), 1);
            let encoded = serde_json::to_string(&observations).unwrap();
            for secret in [
                "private-provider-call-id",
                "private reviewer failure details",
                "approval_ai_fault",
                "decision-1",
            ] {
                assert!(!encoded.contains(secret));
            }
            decided(event, 2_000, |_| panic!("observer fault"));
            assert_eq!(serde_json::to_string(event).unwrap(), before);
        }
    }

    #[test]
    fn request_aliases_are_bounded_and_disambiguate_run_and_request_parts() {
        assert_ne!(
            ObservationAlias::permission_request("one.two", "three"),
            ObservationAlias::permission_request("one", "two.three")
        );
        for (run, request) in [
            ("", "request"),
            ("run", ""),
            ("run", "private input"),
            ("run", "/private/path"),
            ("run", "request\n"),
        ] {
            assert!(ObservationAlias::permission_request(run, request).is_none());
        }
        assert!(ObservationAlias::permission_request(&"r".repeat(257), "request").is_none());
    }

    fn request(id: &str) -> crate::dynamic_run::PermissionRequest {
        crate::permission_tools::build_permission_request(
            &crate::chat::ToolCall { id:"private-provider-call".into(),name:"request_permissions".into(),
                arguments_json:r#"{"items":[{"item_id":"capture","tool_name":"read_current_screen","exact_input":{"display":"private-display"},"suggested_ttl_seconds":300,"suggested_max_uses":1,"reason":"private-owner-reason"}]}"#.into() },
            &crate::ai_assistant::ai_assistant_provider_registry(),id.into(),1,"2026-10-08T00:00:00Z".into(),
        ).unwrap()
    }

    #[test]
    fn input_revision_and_stop_only_end_their_actual_pending_permission_waits() {
        let mut session = crate::session::PersistedAgentSession::new(
            "run-1",
            "owner",
            "device",
            1,
            desk_agent_protocol::AgentScope {
                granted: vec![],
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-10-08T00:00:00Z",
        );
        session.surface = crate::session::AgentSessionSurface::AiAssistant;
        session.input_revision = 1;
        session.begin_focus_epoch(1, []).unwrap();
        session.permission_requests =
            vec![request("pending"), request("approved"), request("denied")];
        session.permission_requests[1].state = PermissionRequestState::Approved;
        session.permission_requests[2].state = PermissionRequestState::Denied;
        assert_eq!(
            waiting_aliases(&session),
            vec![ObservationAlias::permission_request("run-1", "pending").unwrap()]
        );
        session.input_revision = 2;
        session.begin_focus_epoch(2, []).unwrap();
        let mut fresh = request("fresh");
        fresh.input_revision = 2;
        session.permission_requests.push(fresh);
        let before = serde_json::to_string(&session).unwrap();
        let mut superseded = Vec::new();
        input_superseded(&session, 3_000, |event| superseded.push(event));
        assert_eq!(superseded.len(), 1);
        assert_eq!(
            superseded[0].relation,
            Some(ObservationRelation::ResolveTool(
                ObservationAlias::permission_request("run-1", "pending").unwrap()
            ))
        );
        let ObservationPayload::Tool(tool) = &superseded[0].payload else {
            panic!("tool required")
        };
        assert_eq!(tool.permission, PermissionOutcome::Revoked);
        assert_eq!(tool.conclusion, InputConclusion::Unknown);
        let aliases = waiting_aliases(&session);
        assert_eq!(
            aliases,
            vec![ObservationAlias::permission_request("run-1", "fresh").unwrap()]
        );
        let mut cancelled = Vec::new();
        for alias in aliases {
            ended(alias, PermissionOutcome::Cancelled, 3_100, |event| {
                cancelled.push(event)
            });
        }
        let ObservationPayload::Tool(tool) = &cancelled[0].payload else {
            panic!("tool required")
        };
        assert_eq!(tool.permission, PermissionOutcome::Cancelled);
        assert_eq!(tool.issue, InputIssue::None);
        assert_eq!(tool.conclusion, InputConclusion::Unknown);
        assert_eq!(
            session.permission_requests[1].state,
            PermissionRequestState::Approved
        );
        assert_eq!(
            session.permission_requests[2].state,
            PermissionRequestState::Denied
        );
        input_superseded(&session, 3_000, |_| panic!("observer unavailable"));
        ended(
            ObservationAlias::permission_request("run-1", "fresh").unwrap(),
            PermissionOutcome::Cancelled,
            3_100,
            |_| panic!("observer unavailable"),
        );
        assert_eq!(serde_json::to_string(&session).unwrap(), before);
        let encoded = serde_json::to_string(&(superseded, cancelled)).unwrap();
        for secret in [
            "private-provider-call",
            "private-display",
            "private-owner-reason",
            "exact_input",
        ] {
            assert!(!encoded.contains(secret));
        }
    }

    #[test]
    fn goal_decisions_use_committed_states_and_namespaced_original_aliases_without_content() {
        use crate::goal::{
            GoalLimits, GoalModelBinding, GoalOpenRequest, GoalOpenRequestEvent,
            GoalOpenRequestState,
        };
        let model = GoalModelBinding {
            connection_id: "private-connection".into(),
            connection_revision: 1,
            profile_revision: 1,
            model_id: "private-model-binding".into(),
        };
        for state in [
            GoalOpenRequestState::Pending,
            GoalOpenRequestState::Approved,
            GoalOpenRequestState::Denied,
            GoalOpenRequestState::Expired,
            GoalOpenRequestState::Withdrawn,
        ] {
            let mut request = GoalOpenRequest::new(
                "server-request".into(),
                "server-run".into(),
                "private-owner".into(),
                "private-device".into(),
                "private-source-message".into(),
                1,
                "private-goal-text".into(),
                GoalLimits::default(),
                model.clone(),
                1,
            )
            .unwrap();
            let now = if state == GoalOpenRequestState::Expired {
                request.expires_at_unix_ms
            } else {
                2
            };
            if state != GoalOpenRequestState::Pending {
                let id = GoalOpenRequestEvent::id_for(
                    &request.request_id,
                    state,
                    AgentRunEventKind::GoalOpenDecided,
                )
                .unwrap();
                if state == GoalOpenRequestState::Approved {
                    request
                        .approve(1, &model, "created-goal".into(), id, now, None)
                        .unwrap();
                } else {
                    request.close(state, id, now).unwrap();
                }
            }
            request.validate().unwrap();
            let before = serde_json::to_string(&request).unwrap();
            let mut observations = Vec::new();
            for _ in 0..2 {
                goal_open_closed(&request, now as i64, |event| observations.push(event));
            }
            if state == GoalOpenRequestState::Pending {
                assert!(observations.is_empty());
                continue;
            }
            assert_eq!(observations.len(), 2);
            assert_eq!(observations[0], observations[1]);
            assert_eq!(
                observations[0].relation,
                Some(ObservationRelation::ResolveTool(
                    ObservationAlias::goal_open_request("server-run", "server-request").unwrap(),
                ))
            );
            let expected = match state {
                GoalOpenRequestState::Approved => PermissionOutcome::Approved,
                GoalOpenRequestState::Denied => PermissionOutcome::Denied,
                GoalOpenRequestState::Expired => PermissionOutcome::Expired,
                GoalOpenRequestState::Withdrawn => PermissionOutcome::Revoked,
                GoalOpenRequestState::Pending => unreachable!(),
            };
            let ObservationPayload::Tool(tool) = &observations[0].payload else {
                panic!("permission fact required")
            };
            assert_eq!(tool.permission, expected);
            assert_eq!(tool.conclusion, InputConclusion::Unknown);
            assert_eq!(tool.issue, InputIssue::None);
            assert_eq!(tool.stages.len(), 1);
            let encoded = serde_json::to_string(&observations).unwrap();
            for secret in [
                "private-goal-text",
                "private-source-message",
                "private-owner",
                "private-device",
                "private-connection",
                "private-model-binding",
                "created-goal",
            ] {
                assert!(!encoded.contains(secret));
            }
            goal_open_closed(&request, now as i64, |_| panic!("metrics fault"));
            assert_eq!(serde_json::to_string(&request).unwrap(), before);
        }
        assert_ne!(
            ObservationAlias::permission_request("server-run", "server-request"),
            ObservationAlias::goal_open_request("server-run", "server-request")
        );
        assert_ne!(
            ObservationAlias::directory_request("server-run", "server-request"),
            ObservationAlias::goal_open_request("server-run", "server-request")
        );
        for make in [
            ObservationAlias::permission_request,
            ObservationAlias::goal_open_request,
            ObservationAlias::directory_request,
        ] {
            assert!(make("server-run", "/private/path").is_none());
            assert!(make(&"r".repeat(257), "request").is_none());
            assert!(make("one.two", "three").unwrap().is_bounded());
            assert_ne!(make("one.two", "three"), make("one", "two.three"));
        }
    }

    #[test]
    fn production_sized_request_ids_resolve_without_copying_long_identity_parts() {
        use sha2::Digest;
        let run = crate::conversation_key::derive_conversation_key(
            "owner",
            "device",
            Some("browser-run"),
            "fallback",
        );
        assert_eq!(run.len(), 64);
        let digest = format!("{:x}", sha2::Sha256::digest(b"bounded server identity"));
        for (make, prefix) in [
            (
                ObservationAlias::permission_request as fn(&str, &str) -> Option<ObservationAlias>,
                "permission-request",
            ),
            (ObservationAlias::goal_open_request, "goal-open-request"),
            (ObservationAlias::directory_request, "directory-proposal"),
        ] {
            let request = format!("{prefix}-{digest}");
            assert!(run.len() + request.len() > 120);
            let alias = make(&run, &request).unwrap();
            assert_eq!(make(&run, &request), Some(alias.clone()));
            assert!(alias.is_bounded());
            assert!(!alias.key().contains(&run) && !alias.key().contains(&request));
            assert!(alias.key().len() < 80);
            assert_ne!(
                make(&run, &request),
                make(&run, &format!("{request}-other"))
            );
        }
    }

    #[test]
    fn directory_projection_captures_before_terminal_archival_and_skips_owner_only_or_no_change_updates()
     {
        use crate::file_scope::{
            DirectoryConsentSource, DirectoryProposal,
            transaction::{self, FileScopeMutation, FileScopeUpdate},
        };
        for source in [
            DirectoryConsentSource::ModelProposal,
            DirectoryConsentSource::OwnerSelection,
            DirectoryConsentSource::TaskContract,
        ] {
            for revoke in [false, true] {
                let mut session = crate::session::PersistedAgentSession::new(
                    "server-run",
                    "owner",
                    "device",
                    1,
                    desk_agent_protocol::AgentScope {
                        granted: vec![],
                        mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                        expires_at: None,
                        policy_name: None,
                    },
                    "1970-01-01T00:00:00Z",
                );
                session.adopt_client_metadata(
                    Some("client-run"),
                    crate::session::AgentSessionSurface::AiAssistant,
                );
                session.input_revision = 1;
                session.begin_focus_epoch(1, []).unwrap();
                let subject = session
                    .file_scope_subject("owner", "device", "server-run")
                    .unwrap();
                let proposal = DirectoryProposal {
                    request_id: "server-directory".into(),
                    requested_path: "/private/requested-path".into(),
                    canonical_path: "/private/canonical-path".into(),
                    purpose: "private-directory-purpose".into(),
                    source,
                    directory: desk_agent_protocol::computer_use::ObjectRef {
                        token: "private-directory-token".into(),
                        snapshot_id: "private-directory-generation".into(),
                        object_kind: desk_agent_protocol::computer_use::ObjectKind::Directory,
                        expires_at: String::new(),
                    },
                };
                session
                    .file_scope
                    .propose(&subject, 0, proposal, 1)
                    .unwrap();
                let update = FileScopeUpdate {
                    subject,
                    client_conversation_id: "client-run".into(),
                    client_request_id: "server-owner-decision".into(),
                    expected_revision: session.file_scope.revision(),
                    mutation: if revoke {
                        FileScopeMutation::Revoke {
                            directory_request_id: "server-directory".into(),
                        }
                    } else {
                        FileScopeMutation::Decide {
                            directory_request_id: "server-directory".into(),
                            approve: false,
                        }
                    },
                };
                let original = session.clone();
                let pending = PendingDirectoryUpdate::capture(&session, &update);
                let (next, receipt) = transaction::prepare(&session, &update, 2).unwrap();
                assert!(receipt.changed);
                assert!(
                    !next
                        .file_scope
                        .records()
                        .iter()
                        .any(|record| record.proposal.request_id == "server-directory")
                );
                let mut events = Vec::new();
                pending.submit(&receipt, 2, |event| events.push(event));
                assert_eq!(
                    events.len(),
                    usize::from(source != DirectoryConsentSource::OwnerSelection)
                );
                if let Some(event) = events.first() {
                    assert_eq!(
                        event.relation,
                        Some(ObservationRelation::ResolveTool(
                            ObservationAlias::directory_request("server-run", "server-directory")
                                .unwrap()
                        ))
                    );
                    let ObservationPayload::Tool(tool) = &event.payload else {
                        panic!("directory permission fact required")
                    };
                    assert_eq!(
                        tool.permission,
                        if revoke {
                            PermissionOutcome::Revoked
                        } else {
                            PermissionOutcome::Denied
                        }
                    );
                    assert_eq!(tool.conclusion, InputConclusion::Unknown);
                    assert_eq!(tool.issue, InputIssue::None);
                    assert!(!tool.stages.contains_key(&Stage::Dispatch));
                }
                let mut no_change = receipt.clone();
                no_change.changed = false;
                PendingDirectoryUpdate::capture(&session, &update)
                    .submit(&no_change, 2, |_| panic!("no-change must not submit"));
                PendingDirectoryUpdate::capture(&session, &update)
                    .submit(&receipt, 2, |_| panic!("metrics fault"));
                assert_eq!(session, original);
                let encoded = serde_json::to_string(&events).unwrap();
                for secret in [
                    "private/requested-path",
                    "private/canonical-path",
                    "private-directory-purpose",
                    "private-directory-token",
                    "private-directory-generation",
                    "server-owner-decision",
                ] {
                    assert!(!encoded.contains(secret));
                }
            }
        }
    }

    #[test]
    fn pending_ends_ignore_nonterminal_outcomes_and_isolate_observer_failure() {
        let mut session = crate::session::PersistedAgentSession::new(
            "server-run",
            "owner",
            "device",
            1,
            desk_agent_protocol::AgentScope {
                granted: vec![],
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "1970-01-01T00:00:00Z",
        );
        session.input_revision = 1;
        session.permission_requests.push(request("pending"));
        let before = session.clone();
        for outcome in [
            PermissionOutcome::Approved,
            PermissionOutcome::Denied,
            PermissionOutcome::Waiting,
        ] {
            PendingEnds::waiting(&session, outcome)
                .submit(2, |_| panic!("must not manufacture permission decisions"));
        }
        for outcome in [
            PermissionOutcome::Cancelled,
            PermissionOutcome::Revoked,
            PermissionOutcome::Expired,
            PermissionOutcome::Unavailable,
        ] {
            let mut events = Vec::new();
            PendingEnds::waiting(&session, outcome).submit(2, |event| events.push(event));
            assert_eq!(events.len(), 1);
            PendingEnds::waiting(&session, outcome).submit(2, |_| panic!("metrics fault"));
            assert_eq!(session, before);
        }
    }

    #[test]
    fn root_wait_capture_is_bounded_even_if_preparations_are_repeated() {
        let mut session = crate::session::PersistedAgentSession::new(
            "server-run",
            "owner",
            "device",
            1,
            desk_agent_protocol::AgentScope {
                granted: vec![],
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "1970-01-01T00:00:00Z",
        );
        session.input_revision = 1;
        session.permission_requests = (0..crate::dynamic_run::MAX_PERMISSION_REQUESTS)
            .map(|index| request(&format!("request-{index}")))
            .collect();
        let mut prepared = PendingEnds::default();
        for index in 0..(crate::subagent::SUBAGENT_ROOT_CAPACITY + 5) {
            session.conversation_id = format!("server-run-{index}");
            prepared.extend(PendingEnds::waiting(&session, PermissionOutcome::Cancelled));
        }
        let mut events = Vec::new();
        prepared.submit(1, |event| events.push(event));
        assert_eq!(
            events.len(),
            (crate::subagent::SUBAGENT_ROOT_CAPACITY + 1)
                * crate::dynamic_run::MAX_PERMISSION_REQUESTS
        );
        assert!(events.iter().all(ObservationEvent::is_bounded));
        session.permission_requests.push(request("one-too-many"));
        PendingEnds::waiting(&session, PermissionOutcome::Cancelled)
            .submit(1, |_| panic!("oversized metadata must be ignored"));
    }
}
