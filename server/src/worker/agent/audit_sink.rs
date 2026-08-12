//! Single-machine audit sink.
//!
//! Audit events are kept in the local log rather than a database: each event
//! is written as one structured `log` line through the shared [`AuditSink`]
//! contract.
//!
//! Only content-free fields are logged (the builders already guarantee
//! summaries carry counts / sizes, never raw data).

use std::sync::Arc;

use desk_agent_protocol::audit::{AiAuditEventPayload, AuditEvent, AuditSink};
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};
use tokio::sync::broadcast;

use crate::daemon::manager_link_gate::ManagerLinkGate;

/// Logs each audit event at info level. The fixed field set keeps the line
/// greppable; the raw artifact never appears (see module docs).
pub struct LogAuditSink;

/// Fleet-mode audit sink: logs locally **and**, when the manager link should be
/// connected, reports each event to the manager as an `AiAuditEvent(608)`
/// signaling frame over the outbound lane, where the manager observer persists it
/// into `ai_audit_event`. The manager is the only consumer — the signal server
/// drops the frame and the daemon swallows any echo, so it never re-enters a
/// browser-facing lane (security model §6 / D6).
///
/// The manager report is gated dynamically at record time by the shared
/// [`ManagerLinkGate`]: when the manager link is unset or has been disabled at
/// runtime, the sink stays purely local (equivalent to [`LogAuditSink`]) and
/// emits no frame onto the outbound lane.
pub struct RemoteAuditSink {
    outbound_tx: broadcast::Sender<String>,
    manager_link_gate: Arc<ManagerLinkGate>,
}

impl RemoteAuditSink {
    pub fn new(
        outbound_tx: broadcast::Sender<String>,
        manager_link_gate: Arc<ManagerLinkGate>,
    ) -> Self {
        Self {
            outbound_tx,
            manager_link_gate,
        }
    }
}

#[async_trait::async_trait]
impl AuditSink for RemoteAuditSink {
    async fn record(&self, event: AuditEvent) {
        // Keep the local greppable line too, so a fleet host's log still shows
        // its audit trail even if the manager link is momentarily down.
        log::info!("{}", format_audit_line(&event));
        // Stay purely local when the manager link should not be connected, so a
        // host with the manager connection disabled emits no audit frames.
        if !self.manager_link_gate.should_connect() {
            return;
        }
        let payload = AiAuditEventPayload { event };
        match SignalingModel::new_request(SignalingType::ReportAiAuditEvent, None, Some(&payload)) {
            Ok(model) => match serde_json::to_string(&model) {
                Ok(text) => {
                    let _ = self.outbound_tx.send(text);
                }
                Err(e) => log::warn!("[ai-audit] failed to serialize AiAuditEvent: {e}"),
            },
            Err(e) => log::warn!("[ai-audit] failed to build AiAuditEvent model: {e}"),
        }
    }
}

/// Render one audit event as a fixed, greppable log line. The model accounting
/// (provider / model / adapter / token usage) is included so the trail shows
/// **which** wire adapter actually ran a model call — the attribution the
/// model-agnostic path is built around — and is auditable per request. These
/// fields are `None` on non-model events.
fn format_audit_line(event: &AuditEvent) -> String {
    format!(
        "[ai-audit] {event_type} request_id={request_id} actor={actor} \
         capability={capability:?} result={result} provider={provider:?} \
         model={model:?} adapter={adapter:?} tokens_in={tokens_in:?} \
         tokens_out={tokens_out:?} duration_ms={duration:?} \
         redactions={redactions:?} summary={summary:?}",
        event_type = event.event_type,
        request_id = event.request_id,
        actor = event.actor_id,
        capability = event.capability,
        result = event.result,
        provider = event.model_provider,
        model = event.model_name,
        adapter = event.adapter,
        tokens_in = event.input_tokens,
        tokens_out = event.output_tokens,
        duration = event.duration_ms,
        redactions = event.redaction_count,
        summary = event.output_summary,
    )
}

#[async_trait::async_trait]
impl AuditSink for LogAuditSink {
    async fn record(&self, event: AuditEvent) {
        log::info!("{}", format_audit_line(&event));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{CallerRef, CallerType};

    fn sample_event() -> AuditEvent {
        let caller = CallerRef {
            caller_type: CallerType::AiModel,
            model_provider: Some("anthropic".to_string()),
            model_name: Some("claude-x".to_string()),
            adapter: Some("lcxl-anthropic".to_string()),
        };
        AuditEvent::model_responded(
            "evt-gate".to_string(),
            "2026-07-09T00:00:00Z".to_string(),
            "req-gate",
            &caller,
            "diagnosis: 0 findings".to_string(),
            Some(1),
            Some(1),
            10,
        )
    }

    #[tokio::test]
    async fn remote_sink_emits_when_manager_link_should_connect() {
        let (tx, mut rx) = broadcast::channel::<String>(4);
        let gate = Arc::new(ManagerLinkGate::new(true));
        let sink = RemoteAuditSink::new(tx, gate);
        sink.record(sample_event()).await;
        // A frame reaches the outbound lane for the manager observer.
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn remote_sink_stays_local_when_manager_link_disabled() {
        let (tx, mut rx) = broadcast::channel::<String>(4);
        let gate = Arc::new(ManagerLinkGate::new(false));
        let sink = RemoteAuditSink::new(tx, gate);
        sink.record(sample_event()).await;
        // No frame is emitted onto the outbound lane when the link is disabled.
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    /// A model-responded line carries the provider / adapter / token accounting
    /// so the audit trail attributes the call to a concrete wire adapter.
    #[test]
    fn model_responded_line_includes_provider_adapter_and_tokens() {
        let caller = CallerRef {
            caller_type: CallerType::AiModel,
            model_provider: Some("anthropic".to_string()),
            model_name: Some("claude-x".to_string()),
            adapter: Some("lcxl-anthropic".to_string()),
        };
        let event = AuditEvent::model_responded(
            "evt-1".to_string(),
            "2026-06-14T00:00:00Z".to_string(),
            "req-1",
            &caller,
            "diagnosis: 1 findings, 0 commands parse=structured".to_string(),
            Some(1200),
            Some(80),
            15800,
        );
        let line = format_audit_line(&event);
        assert!(line.contains("provider=Some(\"anthropic\")"), "{line}");
        assert!(line.contains("adapter=Some(\"lcxl-anthropic\")"), "{line}");
        assert!(line.contains("tokens_in=Some(1200)"), "{line}");
        assert!(line.contains("tokens_out=Some(80)"), "{line}");
        assert!(line.contains("parse=structured"), "{line}");
    }
}
