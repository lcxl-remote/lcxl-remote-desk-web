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

#[async_trait::async_trait]
impl AuditSink for LogAuditSink {
    async fn record(&self, event: AuditEvent) {
        log::info!(
            "[ai-audit] {event_type} request_id={request_id} actor={actor} \
             capability={capability:?} result={result} duration_ms={duration:?} \
             redactions={redactions:?} summary={summary:?}",
            event_type = event.event_type,
            request_id = event.request_id,
            actor = event.actor_id,
            capability = event.capability,
            result = event.result,
            duration = event.duration_ms,
            redactions = event.redaction_count,
            summary = event.output_summary,
        );
    }
}
