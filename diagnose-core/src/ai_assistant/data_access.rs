//! Frozen owner retrieval scope and task inheritance, independent of device grants.

use super::history::AssistantRetrievalBudget;
use crate::session::{AgentSessionSurface, PersistedAgentSession, TriggerOrigin};
use desk_agent_protocol::ai_assistant::{
    history::{AssistantDeviceScope, HistoryAccessScope},
    source::AssistantSourceKind,
    user_instructions::AssistantDataSettings,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantDataToolAccess {
    pub settings: AssistantDataSettings,
    pub policy_revision: u64,
    pub memory_state_generation: u64,
    pub control_generation: u64,
}

impl AssistantDataToolAccess {
    pub fn validate(&self) -> Result<(), &'static str> {
        super::settings::validate_settings(&self.settings)?;
        if self.policy_revision == 0
            || self.memory_state_generation == 0
            || self.control_generation == 0
        {
            return Err("invalid assistant data access snapshot");
        }
        Ok(())
    }
}

/// The retrieval scope a cached read, query or export was admitted under.
/// Compared by value; any component change prevents reuse.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantScope {
    pub actor_id: String,
    pub conversation_id: String,
    pub source_device_ref: String,
    pub source_instance_id: Option<String>,
    pub retrieval_episode_id: String,
    pub retrieval_task_id: Option<String>,
    pub history_access_scope: Option<Box<HistoryAccessScope>>,
    pub memory_access_scope: Option<Box<super::memory_access::MemoryAccessScope>>,
    pub instructions_revision: u64,
}

pub fn assistant_scope(
    session: &PersistedAgentSession,
) -> Result<Box<AssistantScope>, &'static str> {
    let data = session
        .assistant_data
        .as_ref()
        .ok_or("missing assistant scope state")?;
    Ok(Box::new(AssistantScope {
        actor_id: session.actor_id.clone(),
        conversation_id: session.conversation_id.clone(),
        source_device_ref: data.source_device_ref.clone(),
        source_instance_id: data.source_instance_id.clone(),
        retrieval_episode_id: data.retrieval_budget.episode_id.clone(),
        retrieval_task_id: data.retrieval_task_id.clone(),
        history_access_scope: data.history_access_scope.clone(),
        memory_access_scope: data.memory_access_scope.clone(),
        instructions_revision: data.instructions.revision,
    }))
}

/// A new owner input permits searches of that owner's personal data under the
/// enabled feature. The query still defaults to the current device. This is not
/// an execution grant, and automatic tasks must retain the scope frozen here.
#[allow(clippy::too_many_arguments)]
pub fn owner_history_scope(
    actor: &str,
    root: &str,
    device: &str,
    input_seq: u64,
    accepted_at: u64,
    episode: &str,
    policy_revision: u64,
    settings: &AssistantDataSettings,
    client_timezone: &str,
    temporary: bool,
) -> Result<Option<Box<HistoryAccessScope>>, &'static str> {
    if temporary || !settings.model_history_search_enabled {
        return Ok(None);
    }
    let scope = HistoryAccessScope {
        actor_id: actor.into(),
        root_conversation_id: root.into(),
        task_id: None,
        authorized_input_seq: input_seq,
        accepted_at_unix_ms: accepted_at,
        device_rule: AssistantDeviceScope::AllMyDevices,
        current_device_ref: device.into(),
        allowed_kinds: vec![
            AssistantSourceKind::Input,
            AssistantSourceKind::AssistantMessage,
            AssistantSourceKind::Receipt,
            AssistantSourceKind::NativeStatus,
        ],
        time_range: None,
        timezone: client_timezone.into(),
        policy_revision,
        expires_at_unix_ms: None,
        retrieval_episode_id: episode.into(),
    };
    scope.validate()?;
    Ok(Some(Box::new(scope)))
}

/// Explicit dependency waits continue the original owner's frozen retrieval scope.
/// Command callbacks retain their separate, narrow native context.
pub fn eligible_origin(session: &PersistedAgentSession) -> bool {
    session.surface == AgentSessionSurface::AiAssistant
        && session.agent_role.is_main()
        && matches!(
            session.trigger_origin,
            TriggerOrigin::User
                | TriggerOrigin::PermissionDecision
                | TriggerOrigin::WorkContinuation
                | TriggerOrigin::GoalContinuation
                | TriggerOrigin::ScheduledTask
                | TriggerOrigin::ScheduledContinuation
                | TriggerOrigin::SubAgentCompletion
        )
        && session.assistant_data.as_ref().is_some_and(|data| {
            !data.temporary
                && data.validate().is_ok()
                && (!matches!(
                    session.trigger_origin,
                    TriggerOrigin::GoalContinuation
                        | TriggerOrigin::ScheduledTask
                        | TriggerOrigin::ScheduledContinuation
                ) || data.retrieval_task_id.is_some())
                && (session.trigger_origin != TriggerOrigin::GoalContinuation
                    || session
                        .focus_epoch
                        .goal_segment
                        .as_ref()
                        .is_some_and(|goal| {
                            data.retrieval_task_id.as_deref() == Some(goal.goal_id.as_str())
                        }))
        })
}

/// Reuse saved conversation snapshots on native callbacks without account tools.
/// The host still owns the actor/device/root/version/lease and raw JSON fence.
pub fn retained_context_eligible(session: &PersistedAgentSession) -> bool {
    eligible_origin(session)
        || (session.surface == AgentSessionSurface::AiAssistant
            && session.agent_role.is_main()
            && matches!(session.trigger_origin, TriggerOrigin::WorkCompletion { .. })
            && session
                .assistant_data
                .as_ref()
                .is_some_and(|data| !data.temporary && data.validate().is_ok()))
}

/// This fresh decode validates source bindings. It is not a replacement for any
/// original SELECT, version/lease fence, or raw JSON comparison at a later save.
pub fn validate_persisted_access(
    raw: &str,
    expected: &PersistedAgentSession,
) -> Result<(), &'static str> {
    let persisted = Box::new(
        PersistedAgentSession::decode_json(raw)
            .map_err(|_| "invalid persisted assistant source session")?,
    );
    validate_session_access(&persisted, expected)
}

/// Validate a fresh decode without removing the caller's original SELECT or
/// raw JSON fence. This does not treat a transaction as one read snapshot.
pub fn validate_session_access(
    persisted: &PersistedAgentSession,
    expected: &PersistedAgentSession,
) -> Result<(), &'static str> {
    if persisted.actor_id != expected.actor_id
        || persisted.device_id != expected.device_id
        || persisted.conversation_id != expected.conversation_id
        || persisted.agent_role != expected.agent_role
        || persisted.surface != AgentSessionSurface::AiAssistant
        || persisted.version != expected.version
        || persisted.lease_token != expected.lease_token
        || persisted.control_revision != expected.control_revision
        || persisted.input_revision != expected.input_revision
        || persisted.current_turn_id != expected.current_turn_id
        || persisted.turn_state != expected.turn_state
        || persisted.assistant_data != expected.assistant_data
    {
        return Err("assistant data source session changed");
    }
    Ok(())
}

pub fn usable_history_scope(
    session: &PersistedAgentSession,
    now: u64,
) -> Option<&HistoryAccessScope> {
    if !eligible_origin(session) {
        return None;
    }
    let data = session.assistant_data.as_deref()?;
    let scope = data.history_access_scope.as_deref()?;
    if scope.actor_id != session.actor_id
        || scope.root_conversation_id != session.conversation_id
        || scope.current_device_ref != data.source_device_ref
        || scope.task_id != data.retrieval_task_id
        || scope.retrieval_episode_id != data.retrieval_budget.episode_id
        || scope.expires_at_unix_ms.is_some_and(|until| until <= now)
        || (matches!(
            session.trigger_origin,
            TriggerOrigin::GoalContinuation
                | TriggerOrigin::ScheduledTask
                | TriggerOrigin::ScheduledContinuation
        ) && scope.task_id.is_none())
    {
        return None;
    }
    Some(scope)
}

/// Captured before accepting a correction, then selected only after the original
/// active-goal SELECT and subject/version checks have run in the same transaction.
pub struct TaskRetrievalSnapshot {
    pub budget: AssistantRetrievalBudget,
    pub scope: Option<Box<HistoryAccessScope>>,
    pub memory_scope: Option<Box<super::memory_access::MemoryAccessScope>>,
    pub task_id: Option<String>,
}

pub fn task_retrieval_snapshot(
    session: &PersistedAgentSession,
) -> Option<Box<TaskRetrievalSnapshot>> {
    session.assistant_data.as_ref().map(|data| {
        Box::new(TaskRetrievalSnapshot {
            budget: data.retrieval_budget.clone(),
            scope: data.history_access_scope.clone(),
            memory_scope: data.memory_access_scope.clone(),
            task_id: data.retrieval_task_id.clone(),
        })
    })
}

pub fn bind_opened_task(
    session: &mut PersistedAgentSession,
    task_id: &str,
    deadline: u64,
) -> Result<(), &'static str> {
    desk_agent_protocol::ai_assistant::source::validate_source_id(task_id)?;
    let Some(data) = &mut session.assistant_data else {
        return Err("missing task retrieval state");
    };
    data.validate()?;
    if deadline <= data.current_input_recorded_at_unix_ms || deadline > i64::MAX as u64 {
        return Err("invalid task retrieval deadline");
    }
    if data
        .retrieval_task_id
        .as_ref()
        .is_some_and(|id| id != task_id)
    {
        return Err("retrieval budget belongs to another task");
    }
    let mut history = data.history_access_scope.clone();
    let mut memory = data.memory_access_scope.clone();
    if let Some(scope) = &mut history {
        if scope.task_id.as_ref().is_some_and(|id| id != task_id)
            || scope.actor_id != session.actor_id
            || scope.root_conversation_id != session.conversation_id
        {
            return Err("task history scope belongs to another source");
        }
        let mut candidate = (**scope).clone();
        candidate.task_id = Some(task_id.into());
        candidate.expires_at_unix_ms = Some(
            candidate
                .expires_at_unix_ms
                .map_or(deadline, |old| old.min(deadline)),
        );
        candidate.validate()?;
        **scope = candidate;
    }
    if let Some(scope) = &mut memory {
        if scope.task_id.as_ref().is_some_and(|id| id != task_id)
            || scope.actor_id != session.actor_id
            || scope.root_conversation_id != session.conversation_id
        {
            return Err("task memory scope belongs to another source");
        }
        scope.task_id = Some(task_id.into());
        scope.expires_at_unix_ms = Some(
            scope
                .expires_at_unix_ms
                .map_or(deadline, |old| old.min(deadline)),
        );
        scope.validate()?;
    }
    data.history_access_scope = history;
    data.memory_access_scope = memory;
    data.retrieval_task_id = Some(task_id.into());
    Ok(())
}

pub fn preserve_active_task(
    session: &mut PersistedAgentSession,
    previous: Option<Box<TaskRetrievalSnapshot>>,
    task_id: &str,
    deadline: u64,
) -> Result<(), &'static str> {
    let mut previous = previous.ok_or("active task has no original retrieval budget")?;
    if deadline == 0 || deadline > i64::MAX as u64 {
        return Err("invalid original task deadline");
    }
    if previous.task_id.as_deref() != Some(task_id) {
        return Err("active task retrieval budget binding changed");
    }
    previous.budget.validate()?;
    let device = &session
        .assistant_data
        .as_ref()
        .ok_or("missing task retrieval state")?
        .source_device_ref;
    if let Some(scope) = &mut previous.scope {
        if scope.task_id.as_deref() != Some(task_id)
            || scope.actor_id != session.actor_id
            || scope.root_conversation_id != session.conversation_id
            || scope.retrieval_episode_id != previous.budget.episode_id
            || &scope.current_device_ref != device
        {
            return Err("active task retrieval binding changed");
        }
        scope.expires_at_unix_ms = Some(
            scope
                .expires_at_unix_ms
                .map_or(deadline, |old| old.min(deadline)),
        );
        scope.validate()?;
    }
    if let Some(scope) = &mut previous.memory_scope {
        if scope.task_id.as_deref() != Some(task_id)
            || scope.actor_id != session.actor_id
            || scope.root_conversation_id != session.conversation_id
            || scope.retrieval_episode_id != previous.budget.episode_id
            || &scope.current_device_ref != device
        {
            return Err("active task memory binding changed");
        }
        scope.expires_at_unix_ms = Some(
            scope
                .expires_at_unix_ms
                .map_or(deadline, |old| old.min(deadline)),
        );
        scope.validate()?;
    }
    let data = session
        .assistant_data
        .as_mut()
        .ok_or("missing task retrieval state")?;
    data.retrieval_budget = previous.budget;
    data.history_access_scope = previous.scope;
    data.memory_access_scope = previous.memory_scope;
    data.retrieval_task_id = previous.task_id;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scope_component_change_prevents_reuse() {
        let mut session = super::super::test_support::personal_session("owner", "root", "device");
        session.assistant_data.as_mut().unwrap().memory_access_scope =
            super::super::memory_access::owner_memory_scope(
                "owner",
                "root",
                "",
                1,
                1000,
                "episode",
                1,
                &AssistantDataSettings::default(),
                false,
            )
            .unwrap();
        let original = assistant_scope(&session).unwrap();
        assert!(original.memory_access_scope.is_some());
        for case in 0..8 {
            let mut changed = session.clone();
            let data = changed.assistant_data.as_mut().unwrap();
            match case {
                0 => changed.actor_id = "another-owner".into(),
                1 => changed.conversation_id = "another-root".into(),
                2 => data.source_device_ref = "another-device".into(),
                3 => data.source_instance_id = Some("another-instance".into()),
                4 => data.retrieval_budget.episode_id = "another-episode".into(),
                5 => data.retrieval_task_id = Some("another-task".into()),
                6 => data.instructions.revision += 1,
                _ => super::super::memory_access::bind_request_memory_devices(
                    &mut changed,
                    ["another-device".to_string()],
                )
                .unwrap(),
            }
            assert_ne!(assistant_scope(&changed).unwrap(), original, "case {case}");
        }
    }

    #[test]
    fn owner_correction_preserves_original_task_budget_scope_and_earlier_deadline() {
        use super::super::history::RetrievalCallKind;
        let mut session = super::super::test_support::personal_session("owner", "root", "device");
        bind_opened_task(&mut session, "goal", 1000).unwrap();
        let budget = &mut session.assistant_data.as_mut().unwrap().retrieval_budget;
        for index in 0..4 {
            budget
                .reserve(
                    &format!("search-{index}"),
                    &"a".repeat(64),
                    RetrievalCallKind::Relevance,
                    16384,
                    100,
                )
                .unwrap();
        }
        budget
            .reserve("read", &"b".repeat(64), RetrievalCallKind::Read, 32768, 100)
            .unwrap();
        budget.settle("read", 640).unwrap();
        let previous = task_retrieval_snapshot(&session);
        let original = session
            .assistant_data
            .as_ref()
            .unwrap()
            .retrieval_budget
            .clone();
        let data = session.assistant_data.as_mut().unwrap();
        data.current_actor_input_seq = 8;
        data.current_input_recorded_at_unix_ms = 200;
        data.retrieval_budget = AssistantRetrievalBudget::new("correction".into()).unwrap();
        data.retrieval_task_id = None;
        data.history_access_scope = owner_history_scope(
            "owner",
            "root",
            "device",
            8,
            200,
            "correction",
            2,
            &AssistantDataSettings::default(),
            "UTC",
            false,
        )
        .unwrap();
        preserve_active_task(&mut session, previous, "goal", 2000).unwrap();
        let data = session.assistant_data.as_mut().unwrap();
        assert_eq!(data.retrieval_budget, original);
        assert_eq!(data.current_actor_input_seq, 8);
        let scope = data.history_access_scope.as_ref().unwrap();
        assert_eq!(scope.authorized_input_seq, 1);
        assert_eq!(scope.policy_revision, 1);
        assert_eq!(scope.expires_at_unix_ms, Some(1000));
        assert!(
            data.retrieval_budget
                .reserve(
                    "next-segment",
                    &"c".repeat(64),
                    RetrievalCallKind::Relevance,
                    16384,
                    100
                )
                .is_err()
        );
        assert!(data.validate().is_ok());
    }

    #[test]
    fn task_binding_does_not_create_a_disabled_history_scope_or_accept_a_different_task() {
        let mut session = super::super::test_support::personal_session("owner", "root", "device");
        session
            .assistant_data
            .as_mut()
            .unwrap()
            .history_access_scope = None;
        let before = session.assistant_data.clone();
        assert!(bind_opened_task(&mut session, "goal", 50).is_err());
        assert_eq!(session.assistant_data, before);
        bind_opened_task(&mut session, "goal", 1000).unwrap();
        let previous = task_retrieval_snapshot(&session);
        let before = session.assistant_data.clone();
        assert!(preserve_active_task(&mut session, previous, "other-goal", 900).is_err());
        assert_eq!(session.assistant_data, before);
        let previous = task_retrieval_snapshot(&session);
        preserve_active_task(&mut session, previous, "goal", 900).unwrap();
        assert!(
            session
                .assistant_data
                .as_ref()
                .unwrap()
                .history_access_scope
                .is_none()
        );
        assert_eq!(
            session
                .assistant_data
                .as_ref()
                .unwrap()
                .retrieval_task_id
                .as_deref(),
            Some("goal")
        );
    }

    fn scope() -> Box<HistoryAccessScope> {
        owner_history_scope(
            "owner",
            "root",
            "device",
            4,
            100,
            "episode",
            1,
            &AssistantDataSettings::default(),
            "UTC",
            false,
        )
        .unwrap()
        .unwrap()
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn owner_scope_is_fixed_at_input_and_disabled_or_temporary_inputs_have_no_history_scope() {
        let original = scope();
        assert_eq!(original.device_rule, AssistantDeviceScope::AllMyDevices);
        assert_eq!(original.authorized_input_seq, 4);
        assert!(original.expires_at_unix_ms.is_none());
        let mut settings = AssistantDataSettings::default();
        settings.model_history_search_enabled = false;
        assert!(
            owner_history_scope(
                "owner", "root", "device", 4, 100, "episode", 1, &settings, "UTC", false
            )
            .unwrap()
            .is_none()
        );
        assert!(
            owner_history_scope(
                "owner",
                "root",
                "device",
                4,
                100,
                "episode",
                1,
                &AssistantDataSettings::default(),
                "UTC",
                true
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn a_task_boundary_can_span_multiple_bounded_time_queries() {
        let mut scope = scope();
        scope.time_range = Some(
            desk_agent_protocol::ai_assistant::history::HistoryTimeRange {
                from_unix_ms: 0,
                until_unix_ms: 365 * 86_400_000,
            },
        );
        assert!(scope.validate().is_ok());
        assert!(
            scope
                .time_range
                .as_ref()
                .unwrap()
                .validate_with_limits(&Default::default())
                .is_err()
        );
    }
    #[test]
    fn client_timezones_keep_relative_dates_distinct_and_follow_regional_dst() {
        let utc = chrono::DateTime::parse_from_rfc3339("2026-10-28T17:30:00Z").unwrap();
        let yesterday = |timezone: &str| {
            let scope = owner_history_scope(
                "owner",
                "root",
                "device",
                1,
                100,
                "episode",
                1,
                &AssistantDataSettings::default(),
                timezone,
                false,
            )
            .unwrap()
            .unwrap();
            utc.with_timezone(&scope.timezone.parse::<chrono_tz::Tz>().unwrap())
                .date_naive()
                .pred_opt()
                .unwrap()
        };
        assert_eq!(yesterday("Asia/Shanghai").to_string(), "2026-10-28");
        assert_eq!(yesterday("America/Los_Angeles").to_string(), "2026-10-27");
        let local = |instant: &str| {
            chrono::DateTime::parse_from_rfc3339(instant)
                .unwrap()
                .with_timezone(&"America/Los_Angeles".parse::<chrono_tz::Tz>().unwrap())
                .to_rfc3339()
        };
        assert!(local("2026-07-01T12:00:00Z").ends_with("-07:00"));
        assert!(local("2026-12-01T12:00:00Z").ends_with("-08:00"));
    }
}
