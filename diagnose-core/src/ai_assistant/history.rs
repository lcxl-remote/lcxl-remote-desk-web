//! Durable retrieval accounting and scope checks shared by both central hosts.

use desk_agent_protocol::ai_assistant::history::{
    AssistantDeviceScope, ConversationSearchRequest, HistoryAccessScope, HistorySort,
};
use serde::{Deserialize, Serialize};

/// Canonical model-neutral tool framing, including escaped JSON text and images.
/// Source/time/cursor metadata is already part of content and counts in full.
pub fn serialized_tool_response_bytes(
    call_id: &str,
    content: &str,
    image: Option<&str>,
) -> Result<u32, &'static str> {
    #[derive(Serialize)]
    struct Response<'a> {
        role: &'static str,
        tool_call_id: &'a str,
        content: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        image_data_url: Option<&'a str>,
    }
    let bytes = serde_json::to_vec(&Response {
        role: "tool",
        tool_call_id: call_id,
        content,
        image_data_url: image,
    })
    .map_err(|_| "invalid assistant retrieval response")?;
    u32::try_from(bytes.len()).map_err(|_| "assistant retrieval response is too large")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalCallKind {
    Relevance,
    TimeEnumeration,
    Read,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetrievalReservation {
    pub logical_call_id: String,
    pub request_sha256: String,
    pub kind: RetrievalCallKind,
    /// First durable admission time; retries never renew a query's lifetime.
    pub reserved_at_unix_ms: u64,
    pub reserved_bytes: u32,
    pub actual_bytes: Option<u32>,
}

impl RetrievalReservation {
    pub fn validate(&self) -> Result<(), &'static str> {
        desk_agent_protocol::ai_assistant::source::validate_source_id(&self.logical_call_id)?;
        if self.request_sha256.len() != 64
            || !self
                .request_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || self.reserved_at_unix_ms == 0
            || self.reserved_at_unix_ms > i64::MAX as u64
            || self.reserved_bytes == 0
            || self
                .actual_bytes
                .is_some_and(|actual| actual > self.reserved_bytes)
        {
            return Err("invalid retrieval reservation state");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantRetrievalBudget {
    #[serde(default)]
    pub limits: desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits,
    pub episode_id: String,
    pub relevance_calls: u16,
    pub time_pages: u16,
    pub read_calls: u16,
    /// Includes outstanding reservations, not just already serialized bodies.
    pub read_bytes: u64,
    pub reservations: Vec<RetrievalReservation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservationAdmission {
    New,
    InFlight,
    Settled,
}

impl AssistantRetrievalBudget {
    pub fn validate(&self) -> Result<(), &'static str> {
        desk_agent_protocol::ai_assistant::source::validate_source_id(&self.episode_id)?;
        self.limits.validate()?;
        if self.reservations.len()
            > (u32::from(self.limits.relevance_calls)
                + u32::from(self.limits.time_pages)
                + u32::from(self.limits.read_calls)) as usize
            || self.relevance_calls > self.limits.relevance_calls
            || self.time_pages > self.limits.time_pages
            || self.read_calls > self.limits.read_calls
            || self.read_bytes > self.limits.read_bytes
        {
            return Err("invalid retrieval budget state");
        }
        let mut seen = std::collections::BTreeSet::new();
        let (mut relevance, mut time, mut reads, mut bytes) = (0_u32, 0_u32, 0_u32, 0_u64);
        for entry in &self.reservations {
            entry.validate()?;
            if entry.reserved_bytes > self.limits.response_bytes
                || !seen.insert(&entry.logical_call_id)
            {
                return Err("invalid retrieval reservation state");
            }
            match entry.kind {
                RetrievalCallKind::Relevance => relevance += 1,
                RetrievalCallKind::TimeEnumeration => time += 1,
                RetrievalCallKind::Read => {
                    reads += 1;
                    bytes += u64::from(entry.actual_bytes.unwrap_or(entry.reserved_bytes));
                }
            }
        }
        if (relevance, time, reads, bytes)
            != (
                u32::from(self.relevance_calls),
                u32::from(self.time_pages),
                u32::from(self.read_calls),
                self.read_bytes,
            )
        {
            return Err("retrieval budget counters do not match reservations");
        }
        Ok(())
    }

    pub fn new(episode_id: String) -> Result<Self, &'static str> {
        Self::with_limits(episode_id, Default::default())
    }

    pub fn with_limits(
        episode_id: String,
        limits: desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits,
    ) -> Result<Self, &'static str> {
        desk_agent_protocol::ai_assistant::source::validate_source_id(&episode_id)?;
        Ok(Self {
            limits,
            episode_id,
            relevance_calls: 0,
            time_pages: 0,
            read_calls: 0,
            read_bytes: 0,
            reservations: Vec::new(),
        })
    }

    /// Hosts commit this mutation under the original root/session CAS before querying.
    pub fn reserve(
        &mut self,
        logical_call_id: &str,
        request_sha256: &str,
        kind: RetrievalCallKind,
        upper_bytes: u32,
        now_unix_ms: u64,
    ) -> Result<ReservationAdmission, &'static str> {
        self.validate()?;
        desk_agent_protocol::ai_assistant::source::validate_source_id(logical_call_id)?;
        if now_unix_ms == 0 || now_unix_ms > i64::MAX as u64 {
            return Err("invalid retrieval admission time");
        }
        if request_sha256.len() != 64 || !request_sha256.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err("invalid retrieval request fingerprint");
        }
        if let Some(existing) = self
            .reservations
            .iter()
            .find(|r| r.logical_call_id == logical_call_id)
        {
            if existing.request_sha256 != request_sha256
                || existing.kind != kind
                || existing.reserved_bytes != upper_bytes
            {
                return Err("retrieval call identity conflict");
            }
            return Ok(if existing.actual_bytes.is_some() {
                ReservationAdmission::Settled
            } else {
                ReservationAdmission::InFlight
            });
        }
        if upper_bytes == 0 || upper_bytes > self.limits.response_bytes {
            return Err("invalid retrieval response reservation");
        }
        match kind {
            RetrievalCallKind::Relevance if self.relevance_calls >= self.limits.relevance_calls => {
                return Err("relevance search budget exhausted");
            }
            RetrievalCallKind::TimeEnumeration if self.time_pages >= self.limits.time_pages => {
                return Err("time enumeration budget exhausted");
            }
            RetrievalCallKind::Read
                if self.read_calls >= self.limits.read_calls
                    || self.read_bytes.saturating_add(u64::from(upper_bytes))
                        > self.limits.read_bytes =>
            {
                return Err("history read budget exhausted");
            }
            _ => {}
        }
        match kind {
            RetrievalCallKind::Relevance => self.relevance_calls += 1,
            RetrievalCallKind::TimeEnumeration => self.time_pages += 1,
            RetrievalCallKind::Read => {
                self.read_calls += 1;
                self.read_bytes += u64::from(upper_bytes);
            }
        }
        self.reservations.push(RetrievalReservation {
            logical_call_id: logical_call_id.into(),
            request_sha256: request_sha256.into(),
            kind,
            reserved_at_unix_ms: now_unix_ms,
            reserved_bytes: upper_bytes,
            actual_bytes: None,
        });
        Ok(ReservationAdmission::New)
    }

    pub fn settle(
        &mut self,
        logical_call_id: &str,
        actual_serialized_bytes: u32,
    ) -> Result<(), &'static str> {
        self.validate()?;
        let reservation = self
            .reservations
            .iter_mut()
            .find(|r| r.logical_call_id == logical_call_id)
            .ok_or("unknown retrieval reservation")?;
        if let Some(actual) = reservation.actual_bytes {
            return if actual == actual_serialized_bytes {
                Ok(())
            } else {
                Err("retrieval settlement conflict")
            };
        }
        if actual_serialized_bytes > reservation.reserved_bytes {
            return Err("retrieval response exceeds reservation");
        }
        if reservation.kind == RetrievalCallKind::Read {
            self.read_bytes -= u64::from(reservation.reserved_bytes - actual_serialized_bytes);
        }
        reservation.actual_bytes = Some(actual_serialized_bytes);
        Ok(())
    }
}

/// Request arguments can narrow, but never create, an original task's scope.
pub fn validate_search_scope(
    scope: &HistoryAccessScope,
    request: &ConversationSearchRequest,
    now_unix_ms: u64,
) -> Result<(), &'static str> {
    request.validate()?;
    scope.validate()?;
    if scope
        .expires_at_unix_ms
        .is_some_and(|until| until <= now_unix_ms)
    {
        return Err("history access scope expired");
    }
    if request.scope == AssistantDeviceScope::AllMyDevices
        && scope.device_rule != AssistantDeviceScope::AllMyDevices
    {
        return Err("history device scope exceeds original authorization");
    }
    if request
        .source_kinds
        .iter()
        .any(|kind| !scope.allowed_kinds.contains(kind))
    {
        return Err("history source kind exceeds original authorization");
    }
    if let Some(allowed) = &scope.time_range {
        let requested = request
            .time_range
            .as_ref()
            .ok_or("history query requires the original time boundary")?;
        if requested.from_unix_ms < allowed.from_unix_ms
            || requested.until_unix_ms > allowed.until_unix_ms
        {
            return Err("history time scope exceeds original authorization");
        }
    }
    Ok(())
}

pub fn search_call_kind(request: &ConversationSearchRequest) -> RetrievalCallKind {
    if request.sort == HistorySort::Time {
        RetrievalCallKind::TimeEnumeration
    } else {
        RetrievalCallKind::Relevance
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framed_bytes_include_json_escaping_metadata_and_image_payloads() {
        let content =
            "{\"source_id\":\"src-1\",\"timezone\":\"Asia/Shanghai\",\"text\":\"first\\nsecond\"}";
        let framed = serde_json::to_vec(&serde_json::json!({
            "role": "tool", "tool_call_id": "call\"1", "content": content,
        }))
        .unwrap();
        assert_eq!(
            serialized_tool_response_bytes("call\"1", content, None).unwrap() as usize,
            framed.len()
        );
        assert!(framed.len() > content.len());
        let image = "data:image/png;base64,abcdefgh";
        let with_image = serialized_tool_response_bytes("call\"1", content, Some(image)).unwrap();
        assert_eq!(with_image as usize, serde_json::to_vec(&serde_json::json!({
            "role": "tool", "tool_call_id": "call\"1", "content": content, "image_data_url": image,
        })).unwrap().len());
        assert!(with_image > framed.len() as u32 + image.len() as u32);
    }

    #[test]
    fn pending_reads_reserve_bytes_and_replay_never_reopens_budget() {
        let mut budget = AssistantRetrievalBudget::new("episode-1".into()).unwrap();
        let hash = "a".repeat(64);
        for n in 0..4 {
            assert_eq!(
                budget
                    .reserve(
                        &format!("read-{n}"),
                        &hash,
                        RetrievalCallKind::Read,
                        32768,
                        100
                    )
                    .unwrap(),
                ReservationAdmission::New
            );
        }
        assert!(
            budget
                .reserve("read-5", &hash, RetrievalCallKind::Read, 1, 100)
                .is_err()
        );
        assert_eq!(
            budget
                .reserve("read-0", &hash, RetrievalCallKind::Read, 32768, 100)
                .unwrap(),
            ReservationAdmission::InFlight
        );
        budget.settle("read-0", 100).unwrap();
        budget.settle("read-0", 100).unwrap();
        assert_eq!(budget.read_bytes, 3 * 32768 + 100);
        assert_eq!(
            budget
                .reserve("read-0", &hash, RetrievalCallKind::Read, 32768, 100)
                .unwrap(),
            ReservationAdmission::Settled
        );
        assert_eq!(budget.read_calls, 4);
        assert!(budget.settle("read-1", 32769).is_err());
        assert_eq!(budget.read_bytes, 3 * 32768 + 100);
    }

    #[test]
    fn persistence_roundtrip_retains_total_call_limits() {
        let mut budget = AssistantRetrievalBudget::new("goal-episode".into()).unwrap();
        let hash = "b".repeat(64);
        for n in 0..4 {
            budget
                .reserve(
                    &format!("search-{n}"),
                    &hash,
                    RetrievalCallKind::Relevance,
                    16384,
                    100,
                )
                .unwrap();
        }
        let mut recovered: AssistantRetrievalBudget =
            serde_json::from_str(&serde_json::to_string(&budget).unwrap()).unwrap();
        assert!(
            recovered
                .reserve(
                    "new-segment-search",
                    &hash,
                    RetrievalCallKind::Relevance,
                    16384,
                    100
                )
                .is_err()
        );
        assert!(
            recovered
                .reserve(
                    "search-0",
                    &"c".repeat(64),
                    RetrievalCallKind::Relevance,
                    16384,
                    100
                )
                .is_err()
        );
    }

    #[test]
    fn reservation_time_survives_recovery_and_retries_with_a_new_clock() {
        let mut budget = AssistantRetrievalBudget::new("episode".into()).unwrap();
        let hash = "d".repeat(64);
        budget
            .reserve("search", &hash, RetrievalCallKind::Relevance, 16384, 1000)
            .unwrap();
        let raw = serde_json::to_string(&budget).unwrap();
        let mut resumed: AssistantRetrievalBudget = serde_json::from_str(&raw).unwrap();
        for now in [500, 2000, 1000 + 31 * 60 * 1000] {
            assert_eq!(
                resumed
                    .reserve("search", &hash, RetrievalCallKind::Relevance, 16384, now)
                    .unwrap(),
                ReservationAdmission::InFlight
            );
            assert_eq!(resumed.reservations[0].reserved_at_unix_ms, 1000);
            assert_eq!(resumed, budget);
        }
        resumed.settle("search", 100).unwrap();
        assert_eq!(
            resumed
                .reserve("search", &hash, RetrievalCallKind::Relevance, 16384, 9999)
                .unwrap(),
            ReservationAdmission::Settled
        );
        assert_eq!(resumed.reservations[0].reserved_at_unix_ms, 1000);
    }

    #[test]
    fn invalid_admission_times_do_not_consume_or_repair_a_budget() {
        let mut budget = AssistantRetrievalBudget::new("episode".into()).unwrap();
        let empty = budget.clone();
        for now in [0, i64::MAX as u64 + 1, u64::MAX] {
            assert!(
                budget
                    .reserve("read", &"a".repeat(64), RetrievalCallKind::Read, 32768, now)
                    .is_err()
            );
            assert_eq!(budget, empty);
        }
        budget
            .reserve(
                "read",
                &"a".repeat(64),
                RetrievalCallKind::Read,
                32768,
                1000,
            )
            .unwrap();
        let mut damaged = budget.clone();
        damaged.reservations[0].reserved_at_unix_ms = 0;
        let before = damaged.clone();
        assert!(damaged.validate().is_err());
        assert!(
            damaged
                .reserve(
                    "read",
                    &"a".repeat(64),
                    RetrievalCallKind::Read,
                    32768,
                    2000
                )
                .is_err()
        );
        assert!(damaged.settle("read", 10).is_err());
        assert_eq!(damaged, before);
        let mut json = serde_json::to_value(&budget).unwrap();
        json["reservations"][0]
            .as_object_mut()
            .unwrap()
            .remove("reserved_at_unix_ms");
        assert!(serde_json::from_value::<AssistantRetrievalBudget>(json).is_err());
    }

    #[test]
    fn configured_episode_allowances_survive_resume_and_keep_spent_reservations() {
        let limits = desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits {
            relevance_calls: 6,
            time_pages: 12,
            read_calls: 10,
            read_bytes: 256 * 1024,
            response_bytes: 64 * 1024,
            query_ttl_ms: 60_000,
            unused_projection_ttl_ms: 3_600_000,
            ..Default::default()
        };
        let mut budget =
            AssistantRetrievalBudget::with_limits("configured-episode".into(), limits).unwrap();
        let hash = "a".repeat(64);
        for i in 0..6 {
            budget
                .reserve(
                    &format!("search-{i}"),
                    &hash,
                    RetrievalCallKind::Relevance,
                    100,
                    1,
                )
                .unwrap();
        }
        assert!(
            budget
                .reserve("search-7", &hash, RetrievalCallKind::Relevance, 100, 1)
                .is_err()
        );
        for i in 0..8 {
            budget
                .reserve(
                    &format!("read-{i}"),
                    &hash,
                    RetrievalCallKind::Read,
                    32768,
                    1,
                )
                .unwrap();
        }
        let encoded = serde_json::to_string(&budget).unwrap();
        let mut resumed: AssistantRetrievalBudget = serde_json::from_str(&encoded).unwrap();
        resumed.validate().unwrap();
        assert_eq!(resumed.limits, limits);
        assert_eq!(
            resumed
                .reserve("search-0", &hash, RetrievalCallKind::Relevance, 100, 2)
                .unwrap(),
            ReservationAdmission::InFlight
        );
        assert!(
            resumed
                .reserve("read-9", &hash, RetrievalCallKind::Read, 32768, 2)
                .is_err()
        );
        resumed.settle("read-0", 0).unwrap();
        resumed
            .reserve("read-9", &hash, RetrievalCallKind::Read, 32768, 2)
            .unwrap();
        assert_eq!(resumed.read_calls, 9);
    }

    #[test]
    fn large_configured_allowances_reject_forged_per_kind_totals_without_counter_overflow() {
        let limits = desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits {
            relevance_calls: u16::MAX,
            time_pages: u16::MAX,
            read_calls: u16::MAX,
            read_bytes: 1_000_000,
            ..Default::default()
        };
        let mut budget =
            AssistantRetrievalBudget::with_limits("large-episode".into(), limits).unwrap();
        budget.reservations = (0..=u16::MAX)
            .map(|i| RetrievalReservation {
                logical_call_id: format!("read-{i}"),
                request_sha256: "a".repeat(64),
                kind: RetrievalCallKind::Read,
                reserved_at_unix_ms: 1,
                reserved_bytes: 4,
                actual_bytes: Some(0),
            })
            .collect();
        assert!(budget.validate().is_err());
    }
    #[test]
    fn large_reply_reservations_keep_original_limits_after_serialization_and_settlement() {
        let limits = desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits {
            response_bytes: 256 * 1024,
            read_bytes: 256 * 1024,
            ..Default::default()
        };
        let mut budget =
            AssistantRetrievalBudget::with_limits("large-replies".into(), limits).unwrap();
        let hash = "a".repeat(64);
        budget
            .reserve(
                "search",
                &hash,
                RetrievalCallKind::Relevance,
                64 * 1024,
                1000,
            )
            .unwrap();
        budget
            .reserve("read", &hash, RetrievalCallKind::Read, 256 * 1024, 1000)
            .unwrap();
        assert!(
            budget
                .reserve("exhausted", &hash, RetrievalCallKind::Read, 1, 1000)
                .is_err()
        );
        let mut resumed: AssistantRetrievalBudget =
            serde_json::from_str(&serde_json::to_string(&budget).unwrap()).unwrap();
        resumed.validate().unwrap();
        assert_eq!(
            resumed
                .reserve("read", &hash, RetrievalCallKind::Read, 256 * 1024, 2000)
                .unwrap(),
            ReservationAdmission::InFlight
        );
        assert!(
            resumed
                .reserve("read", &hash, RetrievalCallKind::Read, 32 * 1024, 2000)
                .is_err()
        );
        assert!(resumed.settle("read", 256 * 1024 + 1).is_err());
        resumed.settle("read", 128 * 1024).unwrap();
        resumed
            .reserve(
                "remaining",
                &hash,
                RetrievalCallKind::Read,
                128 * 1024,
                2000,
            )
            .unwrap();
        resumed.validate().unwrap();
        assert_eq!(resumed.read_bytes, 256 * 1024);
    }
}
