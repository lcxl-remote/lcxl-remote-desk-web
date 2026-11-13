//! Model schedule proposals. A proposal never carries execution grants.
use crate::{
    chat::{ToolCall, ToolSpec},
    session::{AgentSessionSurface, PersistedAgentSession},
};
use desk_agent_protocol::schedule::{
    ScheduleCreationSource, ScheduleCreation, ScheduleRule, ScheduleSpec, ScheduleTimeConfirmation,
    ScheduleTimeConversion, ScheduledTaskKind,
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

pub const REQUEST_SCHEDULE: &str = "request_scheduled_task";

/// Trusted clock context only. Locale and device location are never timezones.
pub fn clock_prompt(session: &PersistedAgentSession, now_unix_ms: u64) -> String {
    if !session.owner_direct_planning() || session.surface != AgentSessionSurface::AiAssistant {
        return String::new();
    }
    let Some(now) = i64::try_from(now_unix_ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
    else {
        return String::new();
    };
    format!(
        "\nSCHEDULE PROPOSAL TIME CONTEXT (server clock): current UTC time is {}. No user timezone has been supplied by this context. Do not derive a timezone from response language, device location, browser references, or provider output. If a user gives a local time without an explicit timezone or UTC offset in their instructions, ask them to specify it before proposing a schedule. For local date/time instructions, pass time_input to the server conversion; never compute UTC yourself. For absolute or recurring requests, use rule only when the user supplied an explicit UTC schedule or an absolute RFC3339 instant with an explicit offset; preserve that offset for server normalization. If their local date/time is ambiguous or nonexistent because of daylight saving, ask them to clarify the intended instant. Recurring rules are fixed UTC, so local display times may change when daylight saving changes. A conversation_resume timer continues this conversation once; the server creates and enables it immediately and the tool result is its receipt. A fresh_task starts a new conversation on every run; the tool waits until the AI reviewer or the owner decides in the application review dialog, and approval enables the task as proposed. Use the returned server decision. Do not ask the owner to confirm in chat; a chat reply such as confirm does not approve anything. Neither kind carries permissions: every run approves its device actions one at a time. Never claim a task is enabled without a server receipt or a current query result. after_confirmation is only for conversation_resume; a fresh_task needs time_input with the owner's explicit timezone, or an explicit UTC rule, so for a fresh_task asked only as a relative time ask the owner for the local time and timezone. For relative conversation_resume requests, use rule kind after_confirmation and convert the actual requested duration to delay_seconds: seconds unchanged, minutes multiplied by 60, hours multiplied by 3600. One minute is 60 seconds, not 300. Never copy an example duration; title, prompt and delay must agree. The server counts the delay from creation and no timezone is needed. Use list_scheduled_tasks to verify current state instead of guessing from receipts. You may cancel only AI-created tasks with cancel_scheduled_task, without additional approval; manual tasks are read-only. Enabled tasks survive subsequent ordinary chat; only explicit task pause or cancellation stops them.\n",
        now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    )
}

pub fn registry() -> Vec<crate::registry::RegisteredTool> {
    std::iter::once(spec())
        .chain(super::management_tools::specs())
        .map(|spec| crate::registry::RegisteredTool {
            effect: if spec.name == super::management_tools::LIST {
                crate::registry::ToolEffect::ScheduleQuery
            } else {
                crate::registry::ToolEffect::SchedulePlanning
            },
            spec,
            required_device_capability: None,
        })
        .collect()
}

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: REQUEST_SCHEDULE.into(),
        description: "Schedule a task. conversation_resume continues this conversation once and must run within the deployment's conversation retention window; the server rejects a later run and reports the current maximum delay in seconds. The server creates and enables a conversation_resume timer immediately and returns its receipt. fresh_task starts a new conversation on every run; this call waits until the AI reviewer or the owner decides, and returns that decision; approval enables the task as proposed. Neither kind grants permissions: every run approves its device actions one at a time. For local times use time_input with the explicit user timezone and reference date; the server converts it. For relative conversation continuations use rule kind after_confirmation with delay_seconds; the server counts from creation. after_confirmation is only for conversation_resume; a fresh_task needs time_input or an explicit UTC rule. A chat reply does not approve a proposal. Use other rule kinds only for explicit UTC input or an absolute RFC3339 instant with an explicit offset; preserve the supplied offset. Supply exactly one of rule or time_input; never guess a timezone or compute UTC yourself. The server binds the current owner, device and conversation.".into(),
        parameters_schema: json!({"type":"object","additionalProperties":false,"required":["kind","title","prompt"],"oneOf":[{"required":["rule"],"not":{"required":["time_input"]}},{"required":["time_input"],"not":{"required":["rule"]}}],
            "if":{"properties":{"kind":{"const":"conversation_resume"}}},
            "then":{"properties":{
                "rule":{"properties":{"kind":{"enum":["once","after_confirmation"]}}},
                "time_input":{"properties":{"rule":{"properties":{"kind":{"const":"once"}}}}}
            }},
            "else":{"properties":{
                "rule":{"properties":{"kind":{"not":{"const":"after_confirmation"}}}}
            }},
            "properties":{
                "target_ref":{"type":"string","description":"One device reference admitted with this request. May be omitted only when the request has one device."},
                "kind":{"type":"string","enum":["conversation_resume","fresh_task"]},
                "title":{"type":"string","minLength":1,"maxLength":240},
                "prompt":{"type":"string","minLength":1,"maxLength":16384},
                "time_input":{"type":"object","additionalProperties":false,"required":["timezone","reference_date","local_time","rule"],"properties":{
                    "timezone":{"type":"string","description":"Explicit owner-specified IANA timezone"},
                    "reference_date":{"type":"string","description":"Explicit local date YYYY-MM-DD"},
                    "local_time":{"type":"string","description":"Local clock HH:mm:ss"},
                    "fold":{"type":["string","null"],"enum":["earlier","later",null],"description":"Only choose when the owner disambiguates a repeated local time"},
                    "rule":{"oneOf":[
                        {"type":"object","additionalProperties":false,"required":["kind"],"properties":{"kind":{"enum":["once","daily"]}}},
                        {"type":"object","additionalProperties":false,"required":["kind","weekdays"],"properties":{"kind":{"const":"weekly"},"weekdays":{"type":"array","items":{"type":"integer","minimum":1,"maximum":7},"minItems":1,"maxItems":7,"uniqueItems":true}}},
                        {"type":"object","additionalProperties":false,"required":["kind","every_seconds"],"properties":{"kind":{"const":"interval"},"every_seconds":{"type":"integer","minimum":60,"maximum":31536000}}}
                    ]}
                }},
                "rule":{"oneOf":[
                    {"type":"object","additionalProperties":false,"required":["kind","delay_seconds"],"properties":{"kind":{"const":"after_confirmation"},"delay_seconds":{"type":"integer","minimum":1,"maximum":31536000}}},
                    {"type":"object","additionalProperties":false,"required":["kind","at"],"properties":{"kind":{"const":"once"},"at":{"type":"string","description":"RFC3339 absolute instant with Z or an explicit supplied offset"}}},
                    {"type":"object","additionalProperties":false,"required":["kind","every_seconds","anchor_at"],"properties":{"kind":{"const":"interval"},"every_seconds":{"type":"integer","minimum":60,"maximum":31536000},"anchor_at":{"type":"string","description":"RFC3339 absolute anchor with Z or an explicit supplied offset"}}},
                    {"type":"object","additionalProperties":false,"required":["kind","utc_time"],"properties":{"kind":{"const":"daily"},"utc_time":{"type":"string","description":"UTC time HH:mm:ss"}}},
                    {"type":"object","additionalProperties":false,"required":["kind","weekdays","utc_time"],"properties":{"kind":{"const":"weekly"},"weekdays":{"type":"array","items":{"type":"integer","minimum":1,"maximum":7},"minItems":1,"maxItems":7,"uniqueItems":true},"utc_time":{"type":"string","description":"UTC time HH:mm:ss; weekdays use Monday=1 through Sunday=7"}}}
                ]}
            }}),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    target_ref: Option<String>,
    kind: ScheduledTaskKind,
    title: String,
    prompt: String,
    rule: Option<ScheduleRule>,
    time_input: Option<ScheduleTimeConversion>,
}

/// The caller must hold the original turn fence when persisting this draft and
/// its tool-result receipt. No model argument can replace this security subject.
pub fn draft(
    session: &PersistedAgentSession,
    call: &ToolCall,
    observation: &crate::model_observability::tool::ToolObservation,
) -> Result<ScheduleCreation, &'static str> {
    use crate::model_observability::{InputIssue, PermissionOutcome, Stage, StageOutcome};
    if call.name != REQUEST_SCHEDULE
        || call.id.is_empty()
        || call.id.len() > 256
        || !session.owner_direct_planning()
        || session.surface != AgentSessionSurface::AiAssistant
        || !session.turn_state.is_active()
        || session.input_revision == 0
        || session.actor_id.is_empty()
        || call.arguments_json.len() > 96 * 1024
    {
        if call.name != REQUEST_SCHEDULE {
            observation.reject(Stage::Exposure, InputIssue::UnknownTool);
        } else if call.id.is_empty() || call.id.len() > 256 {
            observation.reject(Stage::Protocol, InputIssue::InvalidProtocol);
        } else if !session.owner_direct_planning()
            || session.surface != AgentSessionSurface::AiAssistant
            || !session.turn_state.is_active()
            || session.input_revision == 0
            || session.actor_id.is_empty()
        {
            observation.permission(PermissionOutcome::PolicyRejected);
        } else {
            observation.reject(Stage::Preflight, InputIssue::Length);
        }
        return Err("schedule proposal is unavailable");
    }
    let input: Input = serde_json::from_str(&call.arguments_json).map_err(|_| {
        observation.reject(Stage::Preflight, InputIssue::Semantic);
        "invalid schedule proposal"
    })?;
    let explicit = input
        .target_ref
        .as_deref()
        .map(|reference| {
            session
                .assistant_request_device(reference)
                .map(|target| target.device_id.as_str())
                .ok_or("scheduled device is outside the original request")
        })
        .transpose()?;
    let execution_target = session.resolve_assistant_action_device(explicit, None)?;
    if input.title.trim().is_empty()
        || input.title.chars().count() > 240
        || input.title.chars().any(char::is_control)
        || input.prompt.trim().is_empty()
        || input.prompt.chars().count() > 16384
        || input
            .prompt
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        observation.reject(Stage::Preflight, InputIssue::Semantic);
        return Err("invalid schedule proposal");
    }
    let (spec, time_confirmation) =
        resolve_time(input.rule, input.time_input).inspect_err(|_| {
            observation.reject(Stage::Preflight, InputIssue::Combination);
        })?;
    let continuation = input.kind == ScheduledTaskKind::ConversationResume;
    // A timer resumes the conversation the owner sees; a child's would resume
    // only the child.
    if continuation && session.agent_role.binding().is_some() {
        observation.reject(Stage::Preflight, InputIssue::Combination);
        return Err(
            "a delegated request cannot propose conversation_resume; propose fresh_task, or report back so the main conversation proposes the timer",
        );
    }
    if !continuation && matches!(spec.rule, ScheduleRule::AfterConfirmation { .. }) {
        observation.reject(Stage::Preflight, InputIssue::Combination);
        return Err(
            "after_confirmation is only for conversation_resume; a fresh_task needs time_input with the owner's explicit timezone or an explicit UTC rule, so ask the owner when it should run",
        );
    }
    if continuation
        && !matches!(
            spec.rule,
            ScheduleRule::Once { .. } | ScheduleRule::AfterConfirmation { .. }
        )
    {
        observation.reject(Stage::Preflight, InputIssue::Combination);
        return Err(
            "conversation_resume requires a one-time rule (once or after_confirmation); use fresh_task for recurring tasks",
        );
    }
    let identity = serde_json::to_vec(&(
        &session.actor_id,
        &session.device_id,
        &session.conversation_id,
        session.input_revision,
        &call.id,
    ))
    .map_err(|_| "invalid proposal identity")?;
    observation.stage(Stage::Preflight, StageOutcome::Attempted);
    Ok(ScheduleCreation {
        time_confirmation,
        client_create_key: format!("ai-schedule-{:x}", Sha256::digest(identity)),
        kind: input.kind,
        target_device_id: execution_target.to_owned(),
        title: input.title,
        prompt: input.prompt,
        locale: session.response_locale.clone(),
        model_id: None,
        spec,
        source_conversation_id: continuation.then(|| session.conversation_id.clone()),
        requirement_revision: continuation.then_some(session.input_revision),
        creation_source: ScheduleCreationSource::AiProposal,
    })
}

/// Hosts return this when the server clock finds no future run for a draft.
pub const NO_FUTURE_OCCURRENCE: &str = "the schedule time has no future occurrence at the server clock; check the requested time, and ask the owner when unsure";

pub fn unavailable() -> desk_agent_protocol::AgentError {
    desk_agent_protocol::AgentError {
        kind: desk_agent_protocol::AgentErrorKind::PermissionDenied,
        message:
            "The schedule operation could not be completed under the current conversation authority. Query the task again; do not claim it was changed."
                .into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

pub fn invalid_input(message: &'static str) -> desk_agent_protocol::AgentError {
    desk_agent_protocol::AgentError {
        kind: desk_agent_protocol::AgentErrorKind::InvalidInput,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

/// The proposed continuation would run after its conversation expires. The
/// message states the current limit so the model can propose a valid time or
/// explain the limit to the owner.
pub fn exceeds_session_retention(max_delay_seconds: u64) -> desk_agent_protocol::AgentError {
    desk_agent_protocol::AgentError {
        kind: desk_agent_protocol::AgentErrorKind::InvalidInput,
        message: format!(
            "Rejected: a conversation_resume task must run within {max_delay_seconds} seconds from now, because this conversation expires after the deployment's session retention window. Propose an earlier time, or explain this limit to the owner. No task was created."
        ),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

fn resolve_time(
    rule: Option<ScheduleRule>,
    time_input: Option<ScheduleTimeConversion>,
) -> Result<(ScheduleSpec, Option<ScheduleTimeConfirmation>), &'static str> {
    match (rule, time_input) {
        (Some(rule), None) => Ok((
            ScheduleSpec {
                schema_version: 1,
                rule,
            },
            None,
        )),
        (None, Some(input)) => {
            let converted = super::timezone::convert(&input).map_err(
                |_| "invalid or ambiguous local schedule time; ask the owner to clarify",
            )?;
            Ok((
                converted.spec,
                Some(ScheduleTimeConfirmation {
                    input,
                    conversion_version: converted.conversion_version,
                }),
            ))
        }
        _ => Err("provide exactly one UTC rule or local time input"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recurring_rules_require_fresh_tasks_in_schema_and_domain_errors() {
        let mut session =
            crate::ai_assistant::test_support::personal_session("owner", "chat", "device");
        session.input_revision = 1;
        session.bind_assistant_input_targets(&[]).unwrap();
        session.turn_state = crate::session::TurnState::Running;
        for timing in [
            json!({"rule":{"kind":"daily","utc_time":"00:30:00"}}),
            json!({"time_input":{"timezone":"Asia/Shanghai","reference_date":"2026-11-01","local_time":"08:30:00","rule":{"kind":"daily"}}}),
        ] {
            let mut input =
                json!({"kind":"conversation_resume","title":"Report","prompt":"Read-only report"});
            input
                .as_object_mut()
                .unwrap()
                .extend(timing.as_object().unwrap().clone());
            let schema_error = crate::model_input::validate_format_with_schema(
                REQUEST_SCHEDULE,
                &spec().parameters_schema,
                &input,
            )
            .unwrap_err();
            assert!(schema_error.contains("once"), "{schema_error}");
            let mut call = ToolCall {
                id: "call".into(),
                name: REQUEST_SCHEDULE.into(),
                arguments_json: input.to_string(),
            };
            let error = draft(&session, &call, &Default::default()).err().unwrap();
            assert!(error.contains("fresh_task"), "{error}");
            assert_eq!(
                invalid_input(error).kind,
                desk_agent_protocol::AgentErrorKind::InvalidInput
            );
            input["kind"] = json!("fresh_task");
            crate::model_input::validate_format_with_schema(
                REQUEST_SCHEDULE,
                &spec().parameters_schema,
                &input,
            )
            .unwrap();
            call.arguments_json = input.to_string();
            assert_eq!(
                draft(&session, &call, &Default::default()).unwrap().kind,
                ScheduledTaskKind::FreshTask
            );
        }
        for rule in [
            json!({"kind":"after_confirmation","delay_seconds":60}),
            json!({"kind":"once","at":"2026-11-01T00:30:00Z"}),
        ] {
            let input = json!({"kind":"conversation_resume","title":"Report","prompt":"Report","rule":rule});
            crate::model_input::validate_format_with_schema(
                REQUEST_SCHEDULE,
                &spec().parameters_schema,
                &input,
            )
            .unwrap();
        }
    }

    #[test]
    fn after_confirmation_is_only_for_conversation_timers() {
        let mut session =
            crate::ai_assistant::test_support::personal_session("owner", "chat", "device");
        session.input_revision = 1;
        session.bind_assistant_input_targets(&[]).unwrap();
        session.turn_state = crate::session::TurnState::Running;
        let input = json!({"kind":"fresh_task","title":"Check","prompt":"Check the tests",
            "rule":{"kind":"after_confirmation","delay_seconds":3600}});
        crate::model_input::validate_format_with_schema(
            REQUEST_SCHEDULE,
            &spec().parameters_schema,
            &input,
        )
        .unwrap_err();
        let call = ToolCall {
            id: "call".into(),
            name: REQUEST_SCHEDULE.into(),
            arguments_json: input.to_string(),
        };
        // The model is told what to change, not that it lacks authority.
        let error = draft(&session, &call, &Default::default()).err().unwrap();
        assert!(error.contains("only for conversation_resume"), "{error}");
        assert_eq!(
            invalid_input(error).kind,
            desk_agent_protocol::AgentErrorKind::InvalidInput
        );
        let prompt = clock_prompt(&session, 1_794_477_143_202);
        assert!(
            prompt.contains("after_confirmation is only for conversation_resume"),
            "{prompt}"
        );
        assert!(
            spec()
                .description
                .contains("after_confirmation is only for conversation_resume")
        );
    }

    #[test]
    fn schedule_proposals_convert_local_time_and_reject_conflicting_or_ambiguous_inputs() {
        let input: ScheduleTimeConversion = serde_json::from_value(json!({
            "timezone":"Asia/Kathmandu", "reference_date":"2026-09-09", "local_time":"14:00:00", "fold":null,
            "rule":{"kind":"daily"}
        })).unwrap();
        let (spec, confirmation) = resolve_time(None, Some(input.clone())).unwrap();
        assert_eq!(
            serde_json::to_value(&spec.rule).unwrap(),
            json!({"kind":"daily","utc_time":"08:15:00"})
        );
        super::super::timezone::verify_confirmation(&spec, &confirmation.unwrap()).unwrap();
        assert!(resolve_time(Some(spec.rule), Some(input)).is_err());
        assert!(resolve_time(None, None).is_err());
        for (date, clock) in [("2026-11-01", "01:30:00"), ("2026-03-08", "02:30:00")] {
            let input = serde_json::from_value(
                json!({"timezone":"America/Los_Angeles", "reference_date":date,
                "local_time":clock,"fold":null,"rule":{"kind":"once"}}),
            )
            .unwrap();
            assert!(resolve_time(None, Some(input)).is_err());
        }
    }
    #[test]
    fn selectable_schedule_proposals_choose_one_original_target() {
        let mut session = crate::ai_assistant::test_support::personal_session("owner", "chat", "");
        session.assistant_data = None;
        session.input_revision = 1;
        session.turn_state = crate::session::TurnState::Running;
        session
            .bind_assistant_input_targets(&[
                crate::dynamic_run::AssistantInputTarget {
                    reference: "ref-a".into(),
                    device_id: "a".into(),
                    member_generation: Some(1),
                },
                crate::dynamic_run::AssistantInputTarget {
                    reference: "ref-b".into(),
                    device_id: "b".into(),
                    member_generation: Some(2),
                },
            ])
            .unwrap();
        for kind in ["conversation_resume", "fresh_task"] {
            let input = json!({"kind":kind,"target_ref":"ref-b","title":"Later","prompt":"Check B","rule":{"kind":"once","at":"2099-01-01T00:00:00Z"}});
            crate::model_input::validate_format_with_schema(
                REQUEST_SCHEDULE,
                &spec().parameters_schema,
                &input,
            )
            .unwrap();
            let mut call = ToolCall {
                id: "proposal".into(),
                name: REQUEST_SCHEDULE.into(),
                arguments_json: input.to_string(),
            };
            assert_eq!(
                draft(&session, &call, &Default::default())
                    .unwrap()
                    .target_device_id,
                "b"
            );
            let mut invalid = input.clone();
            invalid["target_ref"] = json!("ref-c");
            call.arguments_json = invalid.to_string();
            assert!(draft(&session, &call, &Default::default()).is_err());
            let mut ambiguous = input;
            ambiguous.as_object_mut().unwrap().remove("target_ref");
            call.arguments_json = ambiguous.to_string();
            assert!(draft(&session, &call, &Default::default()).is_err());
        }
    }

    fn delegated(source: crate::subagent::DelegationSource) -> PersistedAgentSession {
        let target = crate::dynamic_run::AssistantInputTarget {
            reference: "ref-b".into(),
            device_id: "b".into(),
            member_generation: None,
        };
        let mut child = PersistedAgentSession::new_subagent(
            "sa_child",
            "owner",
            "",
            1,
            desk_agent_protocol::AgentScope {
                granted: Vec::new(),
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            crate::subagent::DelegatedTaskBinding {
                targets: vec![target.clone()],
                root_conversation_id: "chat".into(),
                group_id: "group".into(),
                task_id: "task".into(),
                source,
                objective: "Check".into(),
                acceptance_criteria: vec!["Report".into()],
                input_revision: 1,
                control_revision: 1,
                source_epoch: 1,
                deadline_ms: 1_799_999_999_999,
            },
            "2026-10-20T00:00:00Z",
        )
        .unwrap();
        child.surface = AgentSessionSurface::AiAssistant;
        child.trigger_origin = crate::session::TriggerOrigin::DelegatedTask;
        child.turn_state = crate::session::TurnState::Running;
        child.bind_assistant_input_targets(&[target]).unwrap();
        child
    }

    #[test]
    fn a_directly_delegated_request_proposes_fresh_tasks_but_not_conversation_timers() {
        let call = |kind: &str| {
            ToolCall {
            id: "proposal".into(),
            name: REQUEST_SCHEDULE.into(),
            arguments_json: json!({"kind":kind,"target_ref":"ref-b","title":"Later","prompt":"Check B","rule":{"kind":"once","at":"2099-01-01T00:00:00Z"}}).to_string(),
        }
        };
        let child = delegated(crate::subagent::DelegationSource::UserInput { input_revision: 1 });
        assert!(child.owner_direct_planning());
        assert!(!clock_prompt(&child, 1).is_empty());
        let fresh = draft(&child, &call("fresh_task"), &Default::default()).unwrap();
        assert_eq!(fresh.target_device_id, "b");
        let resume = draft(&child, &call("conversation_resume"), &Default::default()).unwrap_err();
        assert!(resume.contains("delegated request"), "{resume}");
        // Only a request the owner delegated directly, in its delegated turn.
        let mut resumed = child.clone();
        resumed.trigger_origin = crate::session::TriggerOrigin::PermissionDecision;
        assert!(draft(&resumed, &call("fresh_task"), &Default::default()).is_err());
        let issue = delegated(crate::subagent::DelegationSource::Goal {
            goal_id: "goal".into(),
        });
        assert!(!issue.owner_direct_planning());
        assert!(draft(&issue, &call("fresh_task"), &Default::default()).is_err());
        assert!(clock_prompt(&issue, 1).is_empty());
        let tools = registry();
        let names = |session: &PersistedAgentSession| {
            crate::registry::exposed_for_session(&tools, session)
                .iter()
                .map(|tool| tool.name().to_string())
                .collect::<Vec<_>>()
        };
        assert!(names(&child).iter().any(|name| name == REQUEST_SCHEDULE));
        assert!(!names(&issue).iter().any(|name| name == REQUEST_SCHEDULE));
        // Cancelling AI-created tasks follows the same rule as proposing them.
        let cancel = ToolCall {
            id: "cancel".into(),
            name: super::super::management_tools::CANCEL.into(),
            arguments_json: json!({"schedule_id":"sc1"}).to_string(),
        };
        let unavailable = "schedule management is unavailable";
        let direct = super::super::management_tools::parse(&child, &cancel, &Default::default());
        assert_ne!(direct.err(), Some(unavailable));
        let other = super::super::management_tools::parse(&issue, &cancel, &Default::default());
        assert_eq!(other.err(), Some(unavailable));

        // The proposal keeps the owner data frozen at delegation, not the
        // child's request-bounded scope.
        let mut main = crate::ai_assistant::test_support::personal_session("owner", "chat", "");
        main.input_revision = 1;
        main.trigger_origin = crate::session::TriggerOrigin::User;
        main.bind_assistant_input_targets(&[]).unwrap();
        let seed = crate::ai_assistant::delegated_retrieval::DelegatedAssistantSeed::capture(&main)
            .unwrap();
        let owner = crate::model_message_labels::model_bound_user_message(
            "owner-input".into(),
            "Check B later".into(),
            desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: "connection".into(),
                connection_revision: 1,
                model_id: "model".into(),
                profile_revision: 1,
            },
        )
        .unwrap();
        let creation = crate::subagent::creation::CreationEnvelope {
            root_conversation_id: "chat".into(),
            actor_id: "owner".into(),
            device_id: String::new(),
            targets: child.agent_role.binding().unwrap().targets.clone(),
            main_request_id: None,
            source: crate::subagent::DelegationSource::UserInput { input_revision: 1 },
            parent_input_revision: 1,
            parent_control_revision: 1,
            model_destination: owner.data_envelope.as_ref().unwrap().allowed_destinations[0]
                .clone(),
            owner_requirement: owner,
            scheduled_source: None,
            original_read_context: None,
            assistant_seed: Some(seed.clone()),
        };
        let target = child.admitted_assistant_request_targets().unwrap()[0].clone();
        let consent =
            crate::ai_assistant::scheduled_retrieval::TaskRetrievalConsent::capture_delegated(
                "schedule-1",
                1,
                &child,
                &creation,
                target.clone(),
                None,
                200,
            )
            .unwrap();
        assert_eq!(consent.history_scope, seed.history_scope);
        assert_eq!(consent.memory_scope, seed.memory_scope);
        assert_eq!(consent.assistant_seed.as_deref(), Some(&seed.owner));
        // The data came from the main conversation's delegating input.
        assert_eq!(consent.source_conversation_id.as_deref(), Some("chat"));
        assert_eq!(consent.source_input_revision, Some(1));
        assert!(
            crate::ai_assistant::scheduled_retrieval::TaskRetrievalConsent::capture_delegated(
                "schedule-1",
                1,
                &issue,
                &creation,
                target,
                None,
                200,
            )
            .is_err()
        );
    }
}
