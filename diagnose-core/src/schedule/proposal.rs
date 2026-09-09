//! Model proposals create reviewable drafts, never schedules or execution grants.
use crate::{
    chat::{ToolCall, ToolSpec},
    session::{AgentSessionSurface, PersistedAgentSession, TriggerOrigin},
};
use desk_agent_protocol::schedule::{
    ScheduleCreationSource, ScheduleDraft, ScheduleRule, ScheduleSpec, ScheduleTimeConfirmation,
    ScheduleTimeConversion, ScheduledTaskKind,
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

pub const REQUEST_SCHEDULE: &str = "request_scheduled_task";

/// Trusted clock context only. Locale and device location are never timezones.
pub fn clock_prompt(session: &PersistedAgentSession, now_unix_ms: u64) -> String {
    if session.surface != AgentSessionSurface::DeviceAssistant
        || session.trigger_origin != TriggerOrigin::User
    {
        return String::new();
    }
    let Some(now) = i64::try_from(now_unix_ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
    else {
        return String::new();
    };
    format!(
        "\nSCHEDULE PROPOSAL TIME CONTEXT (server clock): current UTC time is {}. No user timezone has been supplied by this context. Do not derive a timezone from response language, device location, browser references, or provider output. If a user gives a local time without an explicit timezone or UTC offset in their instructions, ask them to specify it before proposing a schedule. For local date/time instructions, pass time_input to the server conversion; never compute UTC yourself. For absolute or recurring requests, use rule only when the user supplied an explicit UTC schedule or an absolute RFC3339 instant with an explicit offset; preserve that offset for server normalization. If their local date/time is ambiguous or nonexistent because of daylight saving, ask them to clarify the intended instant. Recurring rules are fixed UTC, so local display times may change when daylight saving changes. A proposal creates a draft only. The application opens a review dialog; direct the owner to its Confirm and enable button. A chat reply such as confirm does NOT activate the task. Never claim activation without a server activation receipt. For relative requests such as in 5 minutes, use rule kind after_confirmation with delay_seconds=300; the server counts from owner confirmation and no timezone is needed.\n",
        now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    )
}

pub fn registry() -> Vec<crate::registry::RegisteredTool> {
    vec![crate::registry::RegisteredTool {
        spec: spec(),
        required_capability: desk_agent_protocol::Capability::SystemInfo,
        effect: crate::registry::ToolEffect::SchedulePlanning,
    }]
}

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: REQUEST_SCHEDULE.into(),
        description: "Propose a scheduled task for the owner to review. This only creates a draft; it does not enable scheduling or grant any permissions. conversation_resume is a one-time continuation of this conversation. fresh_task starts a new context each time and requires owner-guided rehearsal and authorization before publication. For local times use time_input with the explicit user timezone and reference date; the server converts it. For relative conversation continuations use rule kind after_confirmation with delay_seconds; the server counts from confirmation. Ordinary chat confirmation does not activate a draft; the owner must confirm in the review dialog. Use other rule kinds only for explicit UTC input or an absolute RFC3339 instant with an explicit offset; preserve the supplied offset. Supply exactly one of rule or time_input; never guess a timezone or compute UTC yourself. The server binds the current owner, device and conversation.".into(),
        parameters_schema: json!({"type":"object","additionalProperties":false,"required":["kind","title","prompt"],"oneOf":[{"required":["rule"],"not":{"required":["time_input"]}},{"required":["time_input"],"not":{"required":["rule"]}}],
            "properties":{
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
) -> Result<ScheduleDraft, &'static str> {
    if call.name != REQUEST_SCHEDULE
        || call.id.is_empty()
        || call.id.len() > 256
        || session.surface != AgentSessionSurface::DeviceAssistant
        || session.trigger_origin != TriggerOrigin::User
        || !session.turn_state.is_active()
        || session.input_revision == 0
        || session.actor_id.is_empty()
        || session.device_id.is_empty()
        || call.arguments_json.len() > 24 * 1024
    {
        return Err("schedule proposal is unavailable");
    }
    let input: Input =
        serde_json::from_str(&call.arguments_json).map_err(|_| "invalid schedule proposal")?;
    if input.title.trim().is_empty()
        || input.title.len() > 240
        || input.title.chars().any(char::is_control)
        || input.prompt.trim().is_empty()
        || input.prompt.len() > 16384
        || input
            .prompt
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err("invalid schedule proposal");
    }
    let (spec, time_confirmation) = resolve_time(input.rule, input.time_input)?;
    let continuation = input.kind == ScheduledTaskKind::ConversationResume;
    if continuation
        && !matches!(
            spec.rule,
            ScheduleRule::Once { .. } | ScheduleRule::AfterConfirmation { .. }
        )
    {
        return Err("conversation continuation must be one-time");
    }
    let identity = serde_json::to_vec(&(
        &session.actor_id,
        &session.device_id,
        &session.conversation_id,
        session.input_revision,
        &call.id,
    ))
    .map_err(|_| "invalid proposal identity")?;
    Ok(ScheduleDraft {
        time_confirmation,
        client_create_key: format!("ai-schedule-{:x}", Sha256::digest(identity)),
        kind: input.kind,
        target_device_id: session.device_id.clone(),
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

pub fn unavailable() -> desk_agent_protocol::AgentError {
    desk_agent_protocol::AgentError {
        kind: desk_agent_protocol::AgentErrorKind::PermissionDenied,
        message:
            "The schedule proposal could not be saved under the current conversation authority."
                .into(),
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
}
