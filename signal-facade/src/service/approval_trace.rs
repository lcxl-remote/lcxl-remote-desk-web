//! Bounded best-effort approval capture; persistence never gates a decision.
use crate::model::approval_trace::{ApprovalTracePartDto, ApprovalTraceSettings};
use async_trait::async_trait;
use desk_diagnose_core::seam::TurnSink;
use serde_json::Value;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct TraceIdentity {
    pub id: String,
    pub owner_id: String,
    pub conversation_id: String,
    pub device_id: String,
    pub root_conversation_id: String,
    pub source_kind: String,
    pub source_id: String,
    pub lease_epoch: i64,
    pub model_revision: i64,
    pub created_at_ms: i64,
}
#[derive(Clone, Default)]
pub struct TraceCompletion {
    pub model_name: Option<String>,
    pub protocol: Option<String>,
    pub output_limit: Option<i64>,
    pub stop_reason: Option<String>,
    pub usage_json: Option<String>,
    pub reason: Option<String>,
    pub finished_at_ms: Option<i64>,
}
#[derive(Clone, Copy, Debug)]
pub enum TraceAppendError {
    StorageFull,
    SingleLimit,
    Unavailable,
    Deleted,
}
impl TraceAppendError {
    fn state(self) -> &'static str {
        match self {
            Self::StorageFull => "storage_full",
            Self::SingleLimit => "truncated",
            Self::Unavailable => "unavailable",
            Self::Deleted => "deleted",
        }
    }
}
#[async_trait]
pub trait TraceBackend: Send + Sync {
    async fn begin(&self, id: &TraceIdentity, settings: &ApprovalTraceSettings) -> Result<(), ()>;
    async fn append(
        &self,
        id: &TraceIdentity,
        part: ApprovalTracePartDto,
        settings: &ApprovalTraceSettings,
    ) -> Result<(), TraceAppendError>;
    async fn finish(
        &self,
        id: &TraceIdentity,
        completion: TraceCompletion,
        state: &str,
    ) -> Result<(), ()>;
}
pub struct ApprovalTraceSink {
    sender: mpsc::Sender<ApprovalTracePartDto>,
    completion: Arc<Mutex<TraceCompletion>>,
    partial: Arc<AtomicBool>,
    recording_reason: Arc<Mutex<Option<&'static str>>>,
    sequence: u32,
    remaining: usize,
    enabled: bool,
    stage: String,
    first_response: bool,
    model_end_recorded: bool,
    buffers: std::collections::BTreeMap<String, String>,
    last_flush: std::time::Instant,
    pub tracking: Option<Arc<Mutex<desk_diagnose_core::approval_observation::ReviewTracking>>>,
    pub publish: Option<
        Arc<dyn Fn(desk_diagnose_core::model_observability::ObservationEvent) + Send + Sync>,
    >,
}
impl ApprovalTraceSink {
    pub fn start(
        backend: Arc<dyn TraceBackend>,
        id: TraceIdentity,
        settings: ApprovalTraceSettings,
    ) -> Self {
        let (sender, mut receiver) = mpsc::channel(256);
        let completion = Arc::new(Mutex::new(TraceCompletion::default()));
        let partial = Arc::new(AtomicBool::new(false));
        let recording_reason = Arc::new(Mutex::new(None));
        let reason = recording_reason.clone();
        let snapshot = completion.clone();
        let missing = partial.clone();
        let policy = settings.clone();
        tokio::spawn(async move {
            let available = backend.begin(&id, &policy).await.is_ok();
            while let Some(part) = receiver.recv().await {
                let result = if available {
                    tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        backend.append(&id, part, &policy),
                    )
                    .await
                    .unwrap_or(Err(TraceAppendError::Unavailable))
                } else {
                    Err(TraceAppendError::Unavailable)
                };
                if let Err(error) = result {
                    missing.store(true, Ordering::Relaxed);
                    if let Ok(mut state) = reason.lock() {
                        state.get_or_insert(error.state());
                    }
                }
            }
            if available {
                let value = snapshot.lock().map(|v| v.clone()).unwrap_or_default();
                let state = if !policy.enabled {
                    "disabled"
                } else if missing.load(Ordering::Relaxed) || value.finished_at_ms.is_none() {
                    reason.lock().ok().and_then(|v| *v).unwrap_or("partial")
                } else {
                    "complete"
                };
                let _ = backend.finish(&id, value, state).await;
            }
        });
        Self {
            sender,
            completion,
            partial,
            recording_reason,
            sequence: 0,
            remaining: settings.max_trace_bytes as usize,
            enabled: settings.enabled,
            stage: "preparation".into(),
            first_response: false,
            model_end_recorded: false,
            buffers: Default::default(),
            last_flush: std::time::Instant::now(),
            tracking: None,
            publish: None,
        }
    }
    fn flush_kind(&mut self, kind: &str) {
        let text = std::mem::take(self.buffers.entry(kind.into()).or_default());
        if text.is_empty() {
            return;
        }
        let part = ApprovalTracePartDto {
            sequence: self.sequence,
            kind: kind.into(),
            text,
            at_ms: chrono::Utc::now().timestamp_millis(),
        };
        self.sequence += 1;
        if self.sender.try_send(part).is_err() {
            self.partial.store(true, Ordering::Relaxed);
            if let Ok(mut reason) = self.recording_reason.try_lock() {
                reason.get_or_insert("queue_full");
            }
        }
    }
    fn flush(&mut self) {
        for kind in ["input", "reasoning", "answer", "decision", "event"] {
            self.flush_kind(kind);
        }
        self.last_flush = std::time::Instant::now();
    }
    pub fn text(&mut self, kind: &str, mut text: &str) {
        if !self.enabled || !["input", "reasoning", "answer", "decision", "event"].contains(&kind) {
            return;
        }
        while !text.is_empty() {
            let available = 64 * 1024 - self.buffers.get(kind).map_or(0, String::len);
            let mut bytes = text.len().min(available).min(self.remaining);
            while bytes > 0 && !text.is_char_boundary(bytes) {
                bytes -= 1;
            }
            if bytes == 0 {
                if available < 4 && self.remaining >= 4 {
                    self.flush_kind(kind);
                    continue;
                }
                self.partial.store(true, Ordering::Relaxed);
                if let Ok(mut reason) = self.recording_reason.try_lock() {
                    reason.get_or_insert("truncated");
                }
                break;
            }
            self.buffers
                .entry(kind.into())
                .or_default()
                .push_str(&text[..bytes]);
            self.remaining -= bytes;
            text = &text[bytes..];
            if self
                .buffers
                .get(kind)
                .is_some_and(|v| v.len() >= 64 * 1024 - 4)
            {
                self.flush_kind(kind);
            }
        }
        if self.last_flush.elapsed() >= std::time::Duration::from_millis(250) {
            self.flush();
        }
    }
    pub fn event(&mut self, stage: &str) {
        self.stage = stage.into();
        self.text(
            "event",
            &(serde_json::json!({"stage":stage,"origin":if matches!(stage,"first_response"|"model_finished"|"non_stream_response"){"model"}else{"server"},"at_ms":chrono::Utc::now().timestamp_millis()})
                .to_string()
                + "\n"),
        );
    }
    pub fn storage_unavailable(&mut self) {
        self.enabled = false;
        self.partial.store(true, Ordering::Relaxed);
        if let Ok(mut reason) = self.recording_reason.try_lock() {
            reason.get_or_insert("unavailable");
        }
    }
    pub fn model_started(&self) -> bool {
        self.completion
            .try_lock()
            .is_ok_and(|v| v.model_name.is_some())
    }
    pub fn model_finished(&mut self) {
        if !self.model_end_recorded && self.model_started() {
            self.model_end_recorded = true;
            self.event("model_finished");
        }
    }
    pub fn stage(&self) -> &str {
        &self.stage
    }
    pub fn complete(
        &mut self,
        stop: Option<String>,
        usage: Option<String>,
        reason: Option<String>,
    ) {
        if matches!(reason.as_deref(), Some("transport" | "timeout" | "stream")) {
            self.partial.store(true, Ordering::Relaxed);
        }
        self.model_finished();
        self.flush();
        if let Some(tracking) = &self.tracking
            && let Ok(mut tracking) = tracking.try_lock()
        {
            tracking.snapshot.reason = tracking.snapshot.reason.or_else(|| {
                reason.as_ref().and_then(|r| match r.as_str() {
                    "revalidation" => {
                        Some(desk_diagnose_core::approval_observation::ReviewReason::Revoked)
                    }
                    "cancelled" => {
                        Some(desk_diagnose_core::approval_observation::ReviewReason::Revoked)
                    }
                    _ => serde_json::from_value(serde_json::Value::String(r.clone())).ok(),
                })
            });
        }
        if let Ok(mut state) = self.completion.try_lock() {
            state.stop_reason = stop;
            state.usage_json = usage;
            state.reason = reason;
            state.finished_at_ms = Some(chrono::Utc::now().timestamp_millis());
        } else {
            self.partial.store(true, Ordering::Relaxed);
        }
    }
    pub fn protocol(&mut self, protocol: String) {
        if let Ok(mut state) = self.completion.try_lock() {
            state.protocol = Some(protocol);
        }
    }
}
impl TurnSink for ApprovalTraceSink {
    fn on_request_not_started(
        &mut self,
        reason: desk_diagnose_core::model_observability::NotStartedReason,
    ) {
        use desk_diagnose_core::{
            approval_observation::ReviewReason as R, model_observability::NotStartedReason as N,
        };
        let reason = match reason {
            N::Budget => R::Budget,
            N::Policy | N::Permission => R::Policy,
            N::Cancelled => R::Revoked,
            N::Configuration | N::RequestValidation => R::Configuration,
            N::Admission => R::Unknown,
            N::Unknown => R::Processing,
        };
        if let Some(tracking) = &self.tracking
            && let Ok(mut tracking) = tracking.try_lock()
        {
            tracking.snapshot.reason = Some(reason);
        }
        self.event("provider_not_started");
    }

    fn on_provider_json_response(&mut self, body: &Value) {
        self.event("non_stream_response");
        self.partial.store(true, Ordering::Relaxed);
        if let Some(message) = body.pointer("/choices/0/message") {
            if let Some(text) = message["reasoning_content"].as_str() {
                self.on_reasoning_delta(text);
            }
            if let Some(text) = message["content"].as_str() {
                self.on_text_delta(text);
            }
        }
        if let Some(content) = body["content"].as_array() {
            for block in content {
                if block["type"] == "thinking"
                    && let Some(text) = block["thinking"].as_str()
                {
                    self.on_reasoning_delta(text);
                }
                if block["type"] == "text"
                    && let Some(text) = block["text"].as_str()
                {
                    self.on_text_delta(text);
                }
            }
        }
        self.stage = "non_stream_response".into();
    }

    fn on_provider_request(&mut self, body: &Value) {
        if let Ok(mut state) = self.completion.try_lock() {
            state.model_name = body["model"].as_str().map(str::to_owned);
            state.output_limit = body["max_tokens"]
                .as_i64()
                .or_else(|| body["max_completion_tokens"].as_i64());
        }
        if let Some(tracking) = &self.tracking
            && let Ok(mut tracking) = tracking.try_lock()
        {
            let now = chrono::Utc::now().timestamp_millis();
            tracking.snapshot.provider_started_at_ms.get_or_insert(now);
            if let Some(publish) = &self.publish {
                publish(tracking.event(
                    desk_diagnose_core::model_observability::ObservationPhase::Output,
                    1,
                    now,
                ));
            }
        }
        self.event("provider_started");
        if self.enabled {
            self.text(
                "input",
                &desk_diagnose_core::approval_observation::readable_request(body).to_string(),
            );
        }
    }
    fn on_reasoning_delta(&mut self, delta: &str) {
        if !delta.is_empty() && !self.first_response {
            self.first_response = true;
            self.event("first_response");
        }
        self.text("reasoning", delta);
    }
    fn on_text_delta(&mut self, delta: &str) {
        if !delta.is_empty() && !self.first_response {
            self.first_response = true;
            self.event("first_response");
        }
        self.text("answer", delta);
    }
}

pub fn rates(
    totals: &desk_diagnose_core::model_observability::aggregate::Totals,
) -> crate::model::approval_trace::ApprovalRateDto {
    use desk_diagnose_core::model_observability::aggregate::Count;
    let get = |key| totals.counts.get(&key).copied().unwrap_or(0);
    let (a, d, f) = (
        get(Count::ApprovalApproved),
        get(Count::ApprovalDenied),
        get(Count::ApprovalFault),
    );
    let n = u128::from(a) + u128::from(d) + u128::from(f);
    let ratio = |v: u64| (n > 0).then(|| v as f64 / n as f64);
    crate::model::approval_trace::ApprovalRateDto {
        approved: a.to_string(),
        denied: d.to_string(),
        fault: f.to_string(),
        denominator: n.to_string(),
        approval_rate: ratio(a),
        denial_rate: ratio(d),
        fault_rate: ratio(f),
    }
}

impl Drop for ApprovalTraceSink {
    fn drop(&mut self) {
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_diagnose_core::model_observability::aggregate::{Count, Totals};
    struct Backend {
        parts: Mutex<Vec<ApprovalTracePartDto>>,
        done: Mutex<Option<tokio::sync::oneshot::Sender<String>>>,
        fail: bool,
    }
    #[async_trait]
    impl TraceBackend for Backend {
        async fn begin(&self, _: &TraceIdentity, _: &ApprovalTraceSettings) -> Result<(), ()> {
            Ok(())
        }
        async fn append(
            &self,
            _: &TraceIdentity,
            part: ApprovalTracePartDto,
            _: &ApprovalTraceSettings,
        ) -> Result<(), TraceAppendError> {
            if self.fail {
                return Err(TraceAppendError::Unavailable);
            }
            assert!(part.text.len() <= 65536);
            self.parts.lock().unwrap().push(part);
            Ok(())
        }
        async fn finish(
            &self,
            _: &TraceIdentity,
            value: TraceCompletion,
            state: &str,
        ) -> Result<(), ()> {
            assert_eq!(value.stop_reason.as_deref(), Some("end_turn"));
            self.done
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(state.into())
                .unwrap();
            Ok(())
        }
    }
    fn identity() -> TraceIdentity {
        TraceIdentity {
            id: "candidate".into(),
            owner_id: "owner".into(),
            conversation_id: "conversation".into(),
            device_id: "device".into(),
            root_conversation_id: "conversation".into(),
            source_kind: "permission_item".into(),
            source_id: "source".into(),
            lease_epoch: 1,
            model_revision: 1,
            created_at_ms: 1000,
        }
    }
    #[test]
    fn rates_have_one_denominator_and_no_sample_is_not_zero_percent() {
        let mut totals = Totals::default();
        assert!(rates(&totals).approval_rate.is_none());
        totals.counts.extend([
            (Count::ApprovalApproved, 60),
            (Count::ApprovalDenied, 20),
            (Count::ApprovalFault, 10),
            (Count::ApprovalPending, 2),
            (Count::ApprovalCancelled, 5),
            (Count::ApprovalPolicyBlocked, 3),
        ]);
        let result = rates(&totals);
        assert_eq!(result.denominator, "90");
        assert!((result.approval_rate.unwrap() - 2.0 / 3.0).abs() < 1e-10);
        totals.counts.extend([
            (Count::ApprovalApproved, u64::MAX),
            (Count::ApprovalDenied, u64::MAX),
            (Count::ApprovalFault, u64::MAX),
        ]);
        assert_eq!(
            rates(&totals).denominator,
            (u128::from(u64::MAX) * 3).to_string()
        );
    }
    #[test]
    fn large_unicode_capture_uses_a_normal_two_mib_thread_stack() {
        std::thread::Builder::new().stack_size(2*1024*1024).spawn(||{
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async{
                let (send,receive)=tokio::sync::oneshot::channel();let backend=Arc::new(Backend{parts:Mutex::new(vec![]),done:Mutex::new(Some(send)),fail:false});
                let mut sink=ApprovalTraceSink::start(backend.clone(),identity(),ApprovalTraceSettings::defaults(false));
                sink.on_provider_request(&serde_json::json!({"model":"snapshot-model","max_tokens":65536,"headers":{"authorization":"must-never-store"},"messages":[{"role":"user","content":"中".repeat(700_000)}]}));
                let thought="思考".repeat(40_000);sink.on_reasoning_delta(&thought);
                for _ in 0..1000{sink.on_text_delta("好");}
                sink.complete(Some("end_turn".into()),None,None);drop(sink);
                assert_eq!(tokio::time::timeout(std::time::Duration::from_secs(5),receive).await.unwrap().unwrap(),"complete");
                let parts=backend.parts.lock().unwrap();assert!(parts.len()<100);
                assert_eq!(parts.iter().filter(|p|p.kind=="reasoning").map(|p|p.text.as_str()).collect::<String>(),thought);
                assert_eq!(parts.iter().filter(|p|p.kind=="answer").map(|p|p.text.as_str()).collect::<String>(),"好".repeat(1000));
                assert!(!parts.iter().any(|p|p.text.contains("must-never-store")));
            });
        }).unwrap().join().unwrap();
    }
    #[tokio::test]
    async fn storage_failure_and_capture_limit_do_not_prevent_completion() {
        for fail in [true, false] {
            let (send, receive) = tokio::sync::oneshot::channel();
            let backend = Arc::new(Backend {
                parts: Mutex::new(vec![]),
                done: Mutex::new(Some(send)),
                fail,
            });
            let mut settings = ApprovalTraceSettings::defaults(false);
            settings.max_trace_bytes = 262144;
            let mut sink = ApprovalTraceSink::start(backend.clone(), identity(), settings);
            sink.on_reasoning_delta(&"中".repeat(100_000));
            sink.complete(Some("end_turn".into()), Some("{}".into()), None);
            drop(sink);
            assert_eq!(receive.await.unwrap(), "truncated");
            assert!(
                backend
                    .parts
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|p| p.text.len())
                    .sum::<usize>()
                    <= 262144
            );
        }
    }

    #[tokio::test]
    async fn unexpected_json_response_preserves_readable_thinking_without_signatures() {
        let (send, receive) = tokio::sync::oneshot::channel();
        let backend = Arc::new(Backend {
            parts: Mutex::new(vec![]),
            done: Mutex::new(Some(send)),
            fail: false,
        });
        let mut sink = ApprovalTraceSink::start(
            backend.clone(),
            identity(),
            ApprovalTraceSettings::defaults(false),
        );
        let thought = "think".repeat(10_000);
        sink.on_provider_json_response(&serde_json::json!({"content":[{"type":"thinking","thinking":thought,"signature":"opaque-secret"},{"type":"redacted_thinking","data":"opaque-secret"},{"type":"text","text":"actual reply"}]}));
        sink.complete(Some("end_turn".into()), None, Some("stream".into()));
        drop(sink);
        assert_eq!(receive.await.unwrap(), "partial");
        let parts = backend.parts.lock().unwrap();
        assert_eq!(
            parts
                .iter()
                .filter(|p| p.kind == "reasoning")
                .map(|p| p.text.as_str())
                .collect::<String>(),
            thought
        );
        assert!(!parts.iter().any(|p| p.text.contains("opaque-secret")));
    }

    #[tokio::test]
    async fn typed_admission_rejection_survives_generic_permission_error() {
        use desk_diagnose_core::{
            approval_observation::{ReviewReason, ReviewTracking},
            model_observability::NotStartedReason,
        };
        let (send, receive) = tokio::sync::oneshot::channel();
        let backend = Arc::new(Backend {
            parts: Mutex::new(vec![]),
            done: Mutex::new(Some(send)),
            fail: false,
        });
        let tracking = Arc::new(Mutex::new(ReviewTracking::queued(
            "owner",
            "device",
            "conversation",
            "conversation",
            "delegation",
            1,
            "permission_item",
            "item",
            1000,
        )));
        let mut sink =
            ApprovalTraceSink::start(backend, identity(), ApprovalTraceSettings::defaults(false));
        sink.tracking = Some(tracking.clone());
        sink.on_request_not_started(NotStartedReason::Budget);
        sink.complete(Some("end_turn".into()), None, Some("revalidation".into()));
        drop(sink);
        receive.await.unwrap();
        assert_eq!(
            tracking.lock().unwrap().snapshot.reason,
            Some(ReviewReason::Budget)
        );
    }
}
