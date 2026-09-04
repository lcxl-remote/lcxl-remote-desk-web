//! Audited, fail-closed model seam shared by foreground and completion turns.

use crate::model_dial::SignalModelSeam;
use desk_agent_protocol::{AgentError, AgentErrorKind, data_lineage::DestinationIdentity};
use desk_diagnose_core::{
    model_egress::ModelEgressPolicy,
    seam::{ModelRequest, ModelSeam, TurnSink},
};
use sea_orm::DatabaseConnection;
use sha2::{Digest, Sha256};

fn transport_error(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::TransportError,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

pub(crate) struct MeteredModel {
    pub(crate) inner: SignalModelSeam,
    pub(crate) db: DatabaseConnection,
    pub(crate) model_name: String,
    pub(crate) destination: DestinationIdentity,
    pub(crate) selected_source_tools: std::collections::BTreeSet<String>,
    pub(crate) export_authorization_id: String,
    pub(crate) permission_resume: bool,
    pub(crate) model_call_ordinal: std::sync::atomic::AtomicU64,
}

#[async_trait::async_trait(?Send)]
impl ModelSeam for MeteredModel {
    fn model_egress_policy(&self) -> Result<Option<ModelEgressPolicy>, AgentError> {
        let now_unix_ms = u64::try_from(chrono::Utc::now().timestamp_millis())
            .map_err(|_| transport_error("system clock predates the Unix epoch"))?;
        Ok(Some(ModelEgressPolicy {
            destination: self.destination.clone(),
            selected_source_tools: self.selected_source_tools.clone(),
            export_authorization_id: self.export_authorization_id.clone(),
            now_unix_ms,
            byte_cap: desk_diagnose_core::sink_authorizer::MAX_SINK_BYTES,
            omit_finite_retention_historical_turns: self.permission_resume,
        }))
    }

    async fn context_policy(
        &self,
        requirements: desk_diagnose_core::model_capability::ModelRequirements,
    ) -> Result<desk_diagnose_core::model_context::PinnedContextPolicy, AgentError> {
        self.inner.context_policy(requirements).await
    }

    fn on_model_request_projected(
        &self,
        metrics: desk_diagnose_core::seam::ModelRequestProjectionMetrics,
    ) {
        log::debug!(
            "[device-assistant] model projection messages={} message_json_bytes={} tools={} tool_json_bytes={} registry={} ready={} permission_candidates={} catalog_bytes={} index_bytes={} detail_bytes={} conversation_messages={} session_snapshot_bytes={} attachments={} permission_requests={} pending_work={}",
            metrics.message_count,
            metrics.message_json_bytes,
            metrics.advertised_tool_count,
            metrics.advertised_tool_json_bytes,
            metrics.capability_registry_count,
            metrics.runtime_ready_count,
            metrics.permission_candidate_count,
            metrics.capability_catalog_utf8_bytes,
            metrics.capability_index_utf8_bytes,
            metrics.loaded_capability_detail_utf8_bytes,
            metrics.conversation_message_count,
            metrics.session_snapshot_json_bytes,
            metrics.context_attachment_count,
            metrics.permission_request_count,
            metrics.pending_work_trigger_count,
        );
    }

    async fn call(
        &self,
        request: ModelRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<desk_diagnose_core::chat::ModelTurn, AgentError> {
        let policy = self.model_egress_policy()?.ok_or_else(|| {
            transport_error("device assistant model egress policy is unavailable")
        })?;
        let authorized = policy.authorize_request(request).map_err(|error| {
            log::warn!("[device-assistant] model egress denied: {error}");
            AgentError {
                kind: AgentErrorKind::PermissionDenied,
                message: "The selected context is not authorized for the current AI model.".into(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            }
        })?;
        let model_call_ordinal = self
            .model_call_ordinal
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .saturating_add(1);
        let receipt_id = format!(
            "model-egress-{:x}",
            Sha256::digest(
                format!("{}:{model_call_ordinal}", self.export_authorization_id).as_bytes()
            )
        );
        let egress_store = crate::model_egress_store::SignalModelEgressStore::new(self.db.clone());
        egress_store
            .record_dispatch_intent(
                receipt_id.clone(),
                self.export_authorization_id.clone(),
                model_call_ordinal,
                &authorized.audit,
            )
            .await
            .map_err(|error| {
                log::warn!(
                    "[device-assistant] failed to persist model egress receipt_id={receipt_id}: {error}"
                );
                AgentError {
                    kind: AgentErrorKind::Internal,
                    message: "The AI model request could not be audited safely.".into(),
                    retryable: false,
                    safe_for_model: true,
                    error_code: None,
                }
            })?;
        log::info!(
            "[device-assistant] authorized model egress receipt_id={} destination={:?} envelopes={:?} digests={:?} total_bytes={}",
            receipt_id,
            authorized.audit.destination,
            authorized.audit.envelope_ids,
            authorized.audit.digests_sha256,
            authorized.audit.total_bytes
        );
        let mut turn = match self.inner.call(authorized.request, sink).await {
            Ok(turn) => turn,
            Err(error) => {
                if let Err(audit_error) = egress_store.mark_failed(&receipt_id).await {
                    log::warn!(
                        "[device-assistant] failed to close rejected model egress receipt_id={receipt_id}: {audit_error}"
                    );
                }
                return Err(error);
            }
        };
        if turn.text.trim().is_empty() && turn.tool_calls.is_empty() {
            // There is no model output content to label or export. Close this
            // audited provider call as unusable, then let the pure agent loop
            // apply its single bounded empty-EndTurn recovery. Returning the
            // empty turn is safe: it carries no bytes and is never persisted as
            // an assistant message.
            egress_store
                .mark_failed(&receipt_id)
                .await
                .map_err(|error| {
                    log::warn!(
                        "[device-assistant] failed to close empty model egress receipt_id={receipt_id}: {error}"
                    );
                    AgentError {
                        kind: AgentErrorKind::Internal,
                        message: "The empty AI model response could not be audited safely.".into(),
                        retryable: false,
                        safe_for_model: true,
                        error_code: None,
                    }
                })?;
            crate::agent_runtime::record_usage(&self.db, &self.model_name, &turn.usage).await;
            return Ok(turn);
        }
        // A provider call may outlive an ephemeral input that was valid at
        // dispatch. Re-evaluate retention against the completion clock before
        // accepting the model output or allowing any requested tool call to
        // execute. The egress projector already removes historical inputs that
        // lack bounded model-call headroom; this completion check is the final
        // fail-closed guard for current-turn observations.
        let completion_unix_ms = u64::try_from(chrono::Utc::now().timestamp_millis())
            .map_err(|_| transport_error("system clock predates the Unix epoch"))?;
        let completion_policy = ModelEgressPolicy {
            now_unix_ms: completion_unix_ms,
            ..policy.clone()
        };
        let output_envelope = match completion_policy
            .derive_model_output_envelope(&turn, &authorized.input_envelopes)
        {
            Ok(envelope) => envelope,
            Err(error) => {
                log::warn!("[device-assistant] failed to label model output: {error}");
                if let Err(audit_error) = egress_store.mark_failed(&receipt_id).await {
                    log::warn!(
                        "[device-assistant] failed to close unlabeled model egress receipt_id={receipt_id}: {audit_error}"
                    );
                }
                return Err(AgentError {
                    kind: AgentErrorKind::Internal,
                    message: "The AI model output could not be labeled safely.".into(),
                    retryable: false,
                    safe_for_model: true,
                    error_code: None,
                });
            }
        };
        turn.provider_meta.data_envelope = Some(output_envelope);
        let output_envelope_id = turn
            .provider_meta
            .data_envelope
            .as_ref()
            .expect("model output envelope was just assigned")
            .envelope_id
            .clone();
        egress_store
            .mark_succeeded(&receipt_id, &output_envelope_id)
            .await
            .map_err(|error| {
                log::warn!(
                    "[device-assistant] failed to complete model egress receipt_id={receipt_id}: {error}"
                );
                AgentError {
                    kind: AgentErrorKind::Internal,
                    message: "The AI model response could not be audited safely.".into(),
                    retryable: false,
                    safe_for_model: true,
                    error_code: None,
                }
            })?;
        crate::agent_runtime::record_usage(&self.db, &self.model_name, &turn.usage).await;
        Ok(turn)
    }
}
