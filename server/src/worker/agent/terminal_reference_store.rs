//! Worker-lifetime references for explicitly attached terminal output.
//!
//! The PTY reader keeps only a bounded recent suffix in memory and issues an
//! immutable short-lived reference with each output update. The Assistant can
//! later resolve only a reference the browser explicitly attached; raw output
//! is never persisted in the Agent session and is redacted before it leaves the
//! edge.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Duration, Utc};
use desk_agent_protocol::computer_use::{
    ObjectKind, ObjectRef, TerminalOutputInspectOutput, TerminalOutputInspectParams,
    TerminalOutputProjection,
};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::redaction::{Redactor, RegexRedactor};

const TERMINAL_REF_TTL_SECS: i64 = 5 * 60;
const MAX_RECENT_OUTPUT_BYTES: usize = 16 * 1024;
const MAX_TERMINAL_REFS: usize = 8_192;
const MAX_SELECTED_OUTPUTS: usize = 8;

#[derive(Clone)]
struct StoredOutput {
    snapshot_id: String,
    expires_at: DateTime<Utc>,
    content: String,
}

struct StoreState {
    incarnation: String,
    sequence: u64,
    recent_by_terminal: HashMap<String, String>,
    objects: HashMap<String, StoredOutput>,
}

impl Default for StoreState {
    fn default() -> Self {
        Self {
            incarnation: uuid::Uuid::new_v4().to_string(),
            sequence: 0,
            recent_by_terminal: HashMap::new(),
            objects: HashMap::new(),
        }
    }
}

fn store() -> &'static Mutex<StoreState> {
    static STORE: OnceLock<Mutex<StoreState>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(StoreState::default()))
}

pub fn reset_worker_incarnation() {
    if let Ok(mut state) = store().lock() {
        *state = StoreState::default();
    }
}

pub fn reset_terminal(terminal_id: &str) {
    if let Ok(mut state) = store().lock() {
        state.recent_by_terminal.remove(terminal_id);
    }
}

pub fn close_terminal(terminal_id: &str) {
    reset_terminal(terminal_id);
}

/// Append one PTY output chunk and mint an immutable snapshot of the bounded
/// recent suffix. Terminal output delivery itself must continue if issuing a
/// reference fails, so callers may safely drop the error and omit the field.
pub fn append_and_issue(terminal_id: &str, chunk: &str) -> Result<ObjectRef, AgentError> {
    if terminal_id.trim().is_empty() || chunk.is_empty() {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "terminal output reference requires a live terminal and non-empty output",
            false,
        ));
    }
    let now = Utc::now();
    let mut state = store().lock().map_err(|_| unavailable())?;
    state.objects.retain(|_, object| object.expires_at > now);
    if state.objects.len() >= MAX_TERMINAL_REFS {
        return Err(error(
            AgentErrorKind::OutputLimitExceeded,
            "terminal reference store reached its bounded capacity",
            true,
        ));
    }
    let recent = state
        .recent_by_terminal
        .entry(terminal_id.to_string())
        .or_default();
    recent.push_str(chunk);
    truncate_utf8_prefix(recent, MAX_RECENT_OUTPUT_BYTES);
    let content = recent.clone();
    state.sequence = state.sequence.saturating_add(1);
    let snapshot_id = format!("{}:{}", state.incarnation, state.sequence);
    let token = uuid::Uuid::new_v4().to_string();
    let expires_at = now + Duration::seconds(TERMINAL_REF_TTL_SECS);
    state.objects.insert(
        token.clone(),
        StoredOutput {
            snapshot_id: snapshot_id.clone(),
            expires_at,
            content,
        },
    );
    Ok(ObjectRef {
        token,
        snapshot_id,
        object_kind: ObjectKind::TerminalOutput,
        expires_at: expires_at.to_rfc3339(),
    })
}

pub fn inspect(
    params: &TerminalOutputInspectParams,
) -> Result<TerminalOutputInspectOutput, AgentError> {
    if params.roots.is_empty()
        || params.roots.len() > MAX_SELECTED_OUTPUTS
        || params.max_bytes < 512
        || params.max_bytes > 64 * 1024
    {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "terminal output bounds exceed the selected-reference ceiling",
            false,
        ));
    }
    let redactor = RegexRedactor::new();
    let mut entries = Vec::new();
    let mut total_bytes = 0usize;
    let mut truncated = false;
    for object_ref in &params.roots {
        let stored = resolve(object_ref)?;
        let redacted = redactor.redact(&stored.content).map_err(|_| {
            error(
                AgentErrorKind::PermissionDenied,
                "terminal output could not be redacted safely",
                false,
            )
        })?;
        let remaining = (params.max_bytes as usize).saturating_sub(total_bytes);
        if remaining == 0 {
            truncated = true;
            break;
        }
        let (content, entry_truncated) = truncate_utf8_value(redacted.text, remaining);
        total_bytes = total_bytes.saturating_add(content.len());
        entries.push(TerminalOutputProjection {
            snapshot_id: object_ref.snapshot_id.clone(),
            display_summary: "explicitly attached recent terminal output".into(),
            content,
            redaction_count: redacted.kinds.len() as u32,
            truncated: entry_truncated,
        });
        truncated |= entry_truncated;
        if entry_truncated {
            break;
        }
    }
    Ok(TerminalOutputInspectOutput { entries, truncated })
}

fn resolve(object_ref: &ObjectRef) -> Result<StoredOutput, AgentError> {
    if object_ref.object_kind != ObjectKind::TerminalOutput {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "terminal inspection requires terminal output references",
            false,
        ));
    }
    let now = Utc::now();
    let mut state = store().lock().map_err(|_| unavailable())?;
    state.objects.retain(|_, object| object.expires_at > now);
    let stored = state.objects.get(&object_ref.token).ok_or_else(|| {
        error(
            AgentErrorKind::InvalidInput,
            "terminal output reference is stale or unknown",
            false,
        )
    })?;
    if stored.snapshot_id != object_ref.snapshot_id
        || stored.expires_at.to_rfc3339() != object_ref.expires_at
        || !object_ref.snapshot_id.starts_with(&state.incarnation)
    {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "terminal output reference does not belong to this worker incarnation",
            false,
        ));
    }
    Ok(stored.clone())
}

fn truncate_utf8_prefix(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut start = value.len() - max_bytes;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    value.drain(..start);
}

fn truncate_utf8_value(mut value: String, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value, false);
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    (value, true)
}

fn unavailable() -> AgentError {
    error(
        AgentErrorKind::Internal,
        "terminal reference store is unavailable",
        true,
    )
}

fn error(kind: AgentErrorKind, message: impl Into<String>, retryable: bool) -> AgentError {
    AgentError {
        kind,
        message: message.into(),
        retryable,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn attached_terminal_snapshot_is_bounded_redacted_and_tamper_proof() {
        let _guard = test_lock();
        reset_worker_incarnation();
        reset_terminal("terminal-1");
        let object_ref = append_and_issue(
            "terminal-1",
            "build ok\nAuthorization: Bearer secret-terminal-token\n",
        )
        .unwrap();
        let output = inspect(&TerminalOutputInspectParams {
            roots: vec![object_ref.clone()],
            max_bytes: 4096,
        })
        .unwrap();
        assert_eq!(output.entries.len(), 1);
        assert!(output.entries[0].content.contains("build ok"));
        assert!(!output.entries[0].content.contains("secret-terminal-token"));
        assert!(output.entries[0].redaction_count > 0);

        let mut tampered = object_ref;
        tampered.snapshot_id.push_str("-tampered");
        assert!(
            inspect(&TerminalOutputInspectParams {
                roots: vec![tampered],
                max_bytes: 4096,
            })
            .is_err()
        );
    }

    #[test]
    fn worker_reset_makes_terminal_reference_stale() {
        let _guard = test_lock();
        reset_worker_incarnation();
        let object_ref = append_and_issue("terminal-2", "hello\n").unwrap();
        reset_worker_incarnation();
        assert!(
            inspect(&TerminalOutputInspectParams {
                roots: vec![object_ref],
                max_bytes: 4096,
            })
            .is_err()
        );
    }
}
