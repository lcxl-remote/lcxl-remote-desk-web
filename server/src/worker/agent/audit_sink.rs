//! Single-machine audit sink.
//!
//! Audit events are currently kept in the local log rather than a database: each event
//! is written as one structured `log` line. The database-backed sink (mapping
//! every [`AuditEvent`] onto an `ai_audit_event` row) lands in M2 and reuses the
//! same [`AuditSink`] contract, so the emitter (the worker's
//! [`super::LocalDeviceAgent`]) does not change when persistence arrives.
//!
//! Only content-free fields are logged (the builders already guarantee
//! summaries carry counts / sizes, never raw data).

use desk_agent_protocol::audit::{AuditEvent, AuditSink};

/// Logs each audit event at info level. The fixed field set keeps the line
/// greppable; the raw artifact never appears (see module docs).
pub struct LogAuditSink;

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
