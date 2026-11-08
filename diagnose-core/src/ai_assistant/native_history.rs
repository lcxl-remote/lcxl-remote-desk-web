//! Typed native timing and state, bound to the exact retained projection.
//! These metadata carry no execution handles, source bodies, or new authority.

use desk_agent_protocol::ai_assistant::{
    history::{HistoryTimeRange, NativeStatusProjection},
    source::{AssistantSourceKind, validate_source_id},
};
use serde::{Deserialize, Serialize};

/// Retained result identity is distinct from a history index's native ID.
/// Externalized results keep their original envelope; ambiguous matches cannot navigate.
pub fn original_result_message_id(
    messages: &[crate::chat::ChatMessage],
    envelope_id: &str,
) -> Option<String> {
    use crate::chat::ChatRole;
    if validate_source_id(envelope_id).is_err() {
        return None;
    }
    let mut found = messages.iter().filter(|message| {
        matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput)
            && message
                .raw_result
                .as_ref()
                .map_or(message.data_envelope.as_ref(), |raw| {
                    raw.original_envelope.as_ref()
                })
                .is_some_and(|envelope| envelope.envelope_id == envelope_id)
    });
    let message = found.next()?;
    if found.next().is_some() || validate_source_id(&message.message_id).is_err() {
        return None;
    }
    Some(message.message_id.clone())
}

mod status;
pub use status::NativeStatusHistoryProjection;

mod communication;
pub use communication::{
    NativeSendHistoryContext, NativeSendHistoryError, NativeSendHistoryProjection,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeTimeBasis {
    ProviderAckObserved,
    NativeRecorded,
}

impl NativeTimeBasis {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProviderAckObserved => "provider_ack_observed",
            Self::NativeRecorded => "native_recorded",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeHistoryTime {
    Exact {
        at_unix_ms: u64,
        basis: NativeTimeBasis,
    },
    /// Both bounds are server observations. The upper bound is exclusive,
    /// including the final acknowledgement's millisecond at its own precision.
    Interval {
        from_unix_ms: u64,
        until_unix_ms: u64,
        basis: NativeTimeBasis,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeHistoryTimeColumns {
    pub at_unix_ms: Option<u64>,
    pub from_unix_ms: Option<u64>,
    pub until_unix_ms: Option<u64>,
    pub basis: Option<String>,
}

impl NativeHistoryTime {
    pub fn columns(&self) -> NativeHistoryTimeColumns {
        match *self {
            Self::Exact { at_unix_ms, basis } => NativeHistoryTimeColumns {
                at_unix_ms: Some(at_unix_ms),
                basis: Some(basis.as_str().into()),
                ..Default::default()
            },
            Self::Interval {
                from_unix_ms,
                until_unix_ms,
                basis,
            } => NativeHistoryTimeColumns {
                from_unix_ms: Some(from_unix_ms),
                until_unix_ms: Some(until_unix_ms),
                basis: Some(basis.as_str().into()),
                ..Default::default()
            },
        }
    }
    fn validate(&self, recorded_at: u64) -> Result<(), &'static str> {
        match *self {
            Self::Exact {
                at_unix_ms,
                basis: NativeTimeBasis::NativeRecorded,
            } if at_unix_ms > 0 && at_unix_ms <= recorded_at => Ok(()),
            Self::Interval {
                from_unix_ms,
                until_unix_ms,
                ..
            } if from_unix_ms > 0
                && until_unix_ms
                    <= recorded_at.checked_add(1).ok_or("native timing overflow")? =>
            {
                HistoryTimeRange {
                    from_unix_ms,
                    until_unix_ms,
                }
                .validate()
            }
            _ => Err("invalid original native action time"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeHistoryTaskKind {
    Goal,
    Work,
    Subagent,
    Schedule,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeHistoryStatus {
    pub native_id: String,
    pub kind: NativeHistoryTaskKind,
    pub state: String,
    pub revision: Option<u64>,
    pub observed_at_unix_ms: u64,
}

impl NativeHistoryStatus {
    fn validate(&self, recorded_at: u64) -> Result<(), &'static str> {
        validate_source_id(&self.native_id)?;
        if self
            .revision
            .is_some_and(|revision| revision == 0 || revision > i64::MAX as u64)
            || self.observed_at_unix_ms == 0
            || self.observed_at_unix_ms > recorded_at
            || self.state.is_empty()
            || self.state.len() > 64
            || !self
                .state
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        {
            return Err("invalid original native status");
        }
        Ok(())
    }
    pub fn projection(&self, source_id: &str) -> Result<NativeStatusProjection, &'static str> {
        validate_source_id(source_id)?;
        Ok(NativeStatusProjection {
            native_id: self.native_id.clone(),
            kind: self.kind.as_str().into(),
            state: self.state.clone(),
            revision: self.revision,
            observed_at_unix_ms: self.observed_at_unix_ms,
            historical: true,
            source_id: source_id.into(),
            source_device: None,
        })
    }
}

/// A bounded observation of the root's original native rows. Missing state
/// revisions remain absent, and incomplete inventories cannot prove absence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeHistorySnapshot {
    pub observed_at_unix_ms: u64,
    pub items: Vec<NativeHistoryStatus>,
    pub complete: bool,
}
impl NativeHistorySnapshot {
    pub fn bounded(
        mut items: Vec<NativeHistoryStatus>,
        mut complete: bool,
        recorded_at: u64,
        maximum: usize,
    ) -> Result<Self, &'static str> {
        items.sort_by(|a, b| {
            (
                a.priority(),
                std::cmp::Reverse(a.observed_at_unix_ms),
                a.kind.as_str(),
                &a.native_id,
            )
                .cmp(&(
                    b.priority(),
                    std::cmp::Reverse(b.observed_at_unix_ms),
                    b.kind.as_str(),
                    &b.native_id,
                ))
        });
        if items.len() > maximum {
            items.truncate(maximum);
            complete = false;
        }
        let value = Self {
            observed_at_unix_ms: recorded_at,
            items,
            complete,
        };
        value.validate(recorded_at)?;
        Ok(value)
    }
    pub fn validate(&self, recorded_at: u64) -> Result<(), &'static str> {
        if recorded_at == 0 || self.observed_at_unix_ms != recorded_at {
            return Err("invalid native inventory observation");
        }
        let mut identities = std::collections::BTreeSet::new();
        for item in &self.items {
            item.validate(recorded_at)?;
            if !identities.insert((item.kind.as_str(), &item.native_id)) {
                return Err("duplicate native inventory identity");
            }
        }
        Ok(())
    }
    pub fn summary(
        &self,
        source_id: &str,
        source_revision: u64,
        maximum: usize,
    ) -> Result<desk_agent_protocol::ai_assistant::history::NativeStatusSummary, &'static str> {
        self.validate(self.observed_at_unix_ms)?;
        validate_source_id(source_id)?;
        if source_revision == 0 || source_revision > i64::MAX as u64 {
            return Err("invalid native snapshot source revision");
        }
        let mut ordered = self.items.iter().collect::<Vec<_>>();
        ordered.sort_by(|a, b| {
            (
                a.priority(),
                std::cmp::Reverse(a.observed_at_unix_ms),
                a.kind.as_str(),
                &a.native_id,
            )
                .cmp(&(
                    b.priority(),
                    std::cmp::Reverse(b.observed_at_unix_ms),
                    b.kind.as_str(),
                    &b.native_id,
                ))
        });
        Ok(
            desk_agent_protocol::ai_assistant::history::NativeStatusSummary {
                source_id: source_id.into(),
                source_revision,
                additional_sources: Vec::new(),
                last_recorded_at_unix_ms: self.observed_at_unix_ms,
                items: ordered
                    .iter()
                    .take(maximum)
                    .map(|item| item.projection(source_id))
                    .collect::<Result<_, _>>()?,
                additional_count: self.items.len().saturating_sub(maximum) as u32,
                pending_count: self.items.iter().filter(|item| !item.is_terminal()).count() as u32,
                unknown_count: self.items.iter().filter(|item| item.is_unknown()).count() as u32,
                no_native_task: self.complete && self.items.is_empty(),
                has_more: !self.complete || self.items.len() > maximum,
            },
        )
    }
}
impl NativeHistoryTaskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Goal => "goal",
            Self::Work => "work",
            Self::Subagent => "subagent",
            Self::Schedule => "schedule",
        }
    }
}
impl NativeHistoryStatus {
    pub fn is_unknown(&self) -> bool {
        matches!(
            self.state.as_str(),
            "unknown" | "outcome_unknown" | "unknown_outcome"
        )
    }
    pub fn is_terminal(&self) -> bool {
        match self.kind {
            NativeHistoryTaskKind::Work => matches!(
                self.state.as_str(),
                "done" | "succeeded" | "failed" | "rejected" | "expired" | "cancelled"
            ),
            NativeHistoryTaskKind::Goal | NativeHistoryTaskKind::Subagent => {
                matches!(self.state.as_str(), "completed" | "failed" | "cancelled")
            }
            NativeHistoryTaskKind::Schedule => matches!(
                self.state.as_str(),
                "completed" | "failed" | "cancelled" | "skipped" | "missed" | "expired"
            ),
        }
    }
    fn priority(&self) -> u8 {
        if self.is_unknown() {
            0
        } else if !self.is_terminal() {
            1
        } else {
            2
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeHistoryMetadata {
    pub format_version: u16,
    /// Digest of the original native result/control proof, never its payload.
    pub native_proof_sha256: String,
    pub projected_content_sha256: String,
    pub projection_partial: bool,
    pub time: Option<NativeHistoryTime>,
    pub status_snapshot: Option<NativeHistorySnapshot>,
}

impl NativeHistoryMetadata {
    /// Capture-time check: the metadata must describe exactly this body.
    pub fn validate(
        &self,
        kind: AssistantSourceKind,
        original_hash: &str,
        recorded_at: u64,
    ) -> Result<(), &'static str> {
        if self.projected_content_sha256 != original_hash {
            return Err("invalid original native history binding");
        }
        self.validate_stored(kind, recorded_at)
    }

    /// Read-time check of a stored row; the row's columns are trusted, so the
    /// body binding established at capture is not recomputed.
    pub fn validate_stored(
        &self,
        kind: AssistantSourceKind,
        recorded_at: u64,
    ) -> Result<(), &'static str> {
        if self.format_version != 1
            || recorded_at == 0
            || recorded_at >= i64::MAX as u64
            || !valid_hash(&self.native_proof_sha256)
            || !valid_hash(&self.projected_content_sha256)
        {
            return Err("invalid original native history binding");
        }
        match kind {
            AssistantSourceKind::Input
                if self.time.is_none()
                    && self.status_snapshot.is_none()
                    && !self.projection_partial => {}
            AssistantSourceKind::Receipt if self.status_snapshot.is_none() => {}
            AssistantSourceKind::NativeStatus
                if self.status_snapshot.is_some() && self.time.is_none() => {}
            _ => return Err("native history metadata has the wrong source kind"),
        }
        if let Some(time) = &self.time {
            time.validate(recorded_at)?;
        }
        if let Some(status) = &self.status_snapshot {
            status.validate(recorded_at)?;
        }
        Ok(())
    }
    pub fn columns(&self) -> NativeHistoryTimeColumns {
        self.time
            .as_ref()
            .map(NativeHistoryTime::columns)
            .unwrap_or_default()
    }
}

/// Called for owner and model reads alike. The indexed time columns are
/// derived from the stored metadata in the same row and must agree with it.
pub fn read_metadata(
    kind: AssistantSourceKind,
    recorded_at: u64,
    raw: Option<&str>,
    columns: &NativeHistoryTimeColumns,
) -> Result<Option<NativeHistoryMetadata>, &'static str> {
    let value: Option<NativeHistoryMetadata> = raw
        .map(|raw| {
            let value: NativeHistoryMetadata = serde_json::from_str(raw)
                .map_err(|_| "invalid native history metadata encoding")?;
            value.validate_stored(kind, recorded_at)?;
            Ok::<_, &'static str>(value)
        })
        .transpose()?;
    if value.is_none()
        && matches!(
            kind,
            AssistantSourceKind::Receipt | AssistantSourceKind::NativeStatus
        )
        || value
            .as_ref()
            .map(NativeHistoryMetadata::columns)
            .unwrap_or_default()
            != *columns
    {
        return Err("native history metadata disagrees with its columns");
    }
    Ok(value)
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests;
