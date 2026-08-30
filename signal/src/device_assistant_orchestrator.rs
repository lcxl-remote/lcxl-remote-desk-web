//! OSS Signal's owner-only, read-only Device Assistant orchestrator.

use actix_web::web;
use desk_agent_protocol::agent_event::AgentEvent;
use desk_agent_protocol::computer_use::ComputerUseReadiness;
use desk_agent_protocol::data_lineage::{
    ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance, DestinationIdentity,
    RetentionBoundary, Sensitivity,
};
use desk_agent_protocol::device_assistant::{
    DeviceAssistantAsk, DeviceAssistantContextUpdate, DeviceAssistantContextUpdated,
    DeviceAssistantEvent, DeviceAssistantObjectContextOperation,
    DeviceAssistantObjectContextUpdate, DeviceAssistantObjectContextUpdated,
};
use desk_agent_protocol::provenance::AiProvenance;
use desk_agent_protocol::{AgentError, AgentErrorKind, AgentScope, ExecutionMode};
use desk_diagnose_core::agent_loop::{
    LoopDeps, LoopOutcome, resume_agent_turn_after_permission, run_agent_turn,
};
use desk_diagnose_core::capability_availability::{
    CapabilityAvailability, callable_tools, inventory_snapshot, project_capability_availability,
};
use desk_diagnose_core::chat::{ChatMessage, ChatRole};
use desk_diagnose_core::context_attachment::{
    AttachmentBounds, AttachmentObjectRef, AttachmentRuntimeBinding, AttachmentState,
    CONTEXT_ATTACHMENT_SCHEMA_VERSION, ContextAttachment, ContextAttachmentKind,
    MAX_ATTACHMENT_BYTES, MAX_ATTACHMENT_OBJECTS,
};
use desk_diagnose_core::conversation_key::{
    derive_conversation_key, is_valid_client_conversation_id,
};
use desk_diagnose_core::device_assistant::{
    build_device_assistant_system_message_with_catalog, device_assistant_provider_registry,
};
use desk_diagnose_core::model_capability::{
    ModelCapabilities, apply_model_compatibility, filter_model_compatible_tools,
};
use desk_diagnose_core::model_egress::ModelEgressPolicy;
use desk_diagnose_core::prompt::ResponseFormatSpec;
use desk_diagnose_core::registry::RegisteredTool;
use desk_diagnose_core::seam::{
    ClaimTurnParams, HeartbeatGuard, LeaseHeartbeat, ModelRequest, ModelSeam, SessionSeam, TurnSink,
};
use desk_diagnose_core::session::{AgentSessionSurface, TriggerOrigin};
use desk_diagnose_core::stream::StreamingTurnSink;
use desk_signal_facade::model::connection::{ConnectionState, SharedConnectionMap};
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};
use sea_orm::DatabaseConnection;
use sha2::{Digest, Sha256};

use crate::model_dial::SignalModelSeam;

const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const BUSY_FOLLOWUP_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);
const OSS_ASSISTANT_POLICY_REVISION: i64 = 1;
use desk_diagnose_core::permission_resume::{
    bind_exact_authorization_system_message, model_bound_permission_resume_message,
};

fn extend_fixed_exact_action_capabilities(
    registry: &[RegisteredTool],
    granted: &mut Vec<desk_agent_protocol::Capability>,
) {
    if registry.iter().any(|tool| {
        tool.name() == desk_diagnose_core::device_assistant::EXECUTE_CONFIRMED_UI_ACTION_TOOL
    }) && !granted.contains(&desk_agent_protocol::Capability::DesktopUiActionConfirmed)
    {
        // The model-facing tool remains callable after a context attachment
        // expires so an owner-approved exact grant can resume. The grant store
        // still binds the call to the exact object, input, device, actor, TTL,
        // and one-shot use before the edge sees any action.
        granted.push(desk_agent_protocol::Capability::DesktopUiActionConfirmed);
    }
    if registry.iter().any(|tool| {
        tool.name() == desk_diagnose_core::device_assistant::EXECUTE_CONFIRMED_RAW_INPUT_TOOL
    }) && !granted.contains(&desk_agent_protocol::Capability::DesktopInputFallbackConfirmed)
    {
        granted.push(desk_agent_protocol::Capability::DesktopInputFallbackConfirmed);
    }
}

fn capability_enables_mutation(capability: &desk_agent_protocol::Capability) -> bool {
    matches!(
        capability,
        desk_agent_protocol::Capability::DesktopUiActionConfirmed
            | desk_agent_protocol::Capability::DesktopInputFallbackConfirmed
            | desk_agent_protocol::Capability::FileArtifactCreateConfirmed
            | desk_agent_protocol::Capability::CommunicationLocalDraftCreateConfirmed
            | desk_agent_protocol::Capability::SpreadsheetWorkbookCreateConfirmed
            | desk_agent_protocol::Capability::SpreadsheetFormulaWorkbookCreateConfirmed
            | desk_agent_protocol::Capability::WordDocumentCreateConfirmed
            | desk_agent_protocol::Capability::ShellExecConfirmed
            | desk_agent_protocol::Capability::BrowserPageNavigateConfirmed
            | desk_agent_protocol::Capability::BrowserInputFallbackConfirmed
            | desk_agent_protocol::Capability::BrowserExternalDraftWriteConfirmed
            | desk_agent_protocol::Capability::CommunicationOutlookNewHandoffConfirmed
            | desk_agent_protocol::Capability::SpreadsheetLivePatchConfirmed
            | desk_agent_protocol::Capability::DocumentLivePatchConfirmed
            | desk_agent_protocol::Capability::PresentationLivePatchConfirmed
    )
}

fn has_active_resume_desktop_ui_inspect_grant(
    grants: &[desk_agent_protocol::capability_grant::CapabilityGrant],
    now_unix_ms: u64,
) -> bool {
    grants.iter().any(|grant| {
        grant.provider_id == desk_diagnose_core::device_assistant::DESKTOP_UI_PROVIDER_ID
            && grant.capability_id == desk_diagnose_core::device_assistant::DESKTOP_UI_CAPABILITY_ID
            && grant.tool_name == "inspect_desktop_ui"
            && grant.effect
                == desk_agent_protocol::capability_provider::CapabilityEffect::ReadDevice
            && grant.revoked_at_unix_ms.is_none()
            && grant.expires_at_unix_ms > now_unix_ms
            && grant.remaining_uses > 0
    })
}

fn latest_committed_answer(
    snapshot: &crate::agent_session_store::SessionSnapshot,
) -> Option<String> {
    snapshot
        .messages
        .iter()
        .rev()
        .find(|message| {
            message.role == ChatRole::Assistant
                && message.tool_calls.is_empty()
                && !message.text.trim().is_empty()
        })
        .map(|message| message.text.clone())
}

async fn current_capability_projection(
    connections: &SharedConnectionMap,
    target_connection_id: &str,
    model_capabilities: ModelCapabilities,
) -> (
    desk_diagnose_core::provider_registry::ProviderRegistry,
    Vec<CapabilityAvailability>,
    u64,
    Option<ComputerUseReadiness>,
) {
    let provider_registry = device_assistant_provider_registry();
    let target_is_live = {
        let connections = connections.read().await;
        connections.contains_key(target_connection_id)
    };
    let cached_readiness = if target_is_live {
        crate::computer_use_readiness::global_computer_use_readiness_cache()
            .get_fresh(target_connection_id, chrono::Utc::now())
    } else {
        None
    };
    let readiness = match cached_readiness
        .as_ref()
        .map(|cached| {
            desk_diagnose_core::device_assistant::provider_readiness_reports(&cached.readiness)
        })
        .transpose()
    {
        Ok(Some(readiness)) => readiness,
        Ok(None) => Vec::new(),
        Err(error) => {
            log::warn!("ignoring invalid Device Assistant capability readiness: {error}");
            Vec::new()
        }
    };
    let generated_at_unix_ms = u64::try_from(chrono::Utc::now().timestamp_millis())
        .expect("current time is after the Unix epoch");
    let mut inventory = project_capability_availability(
        &provider_registry,
        desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner,
        generated_at_unix_ms,
        readiness,
    )
    .expect("validated Computer Use readiness must match the static Provider registry");
    apply_model_compatibility(&mut inventory, model_capabilities);
    (
        provider_registry,
        inventory,
        generated_at_unix_ms,
        cached_readiness.map(|cached| cached.readiness),
    )
}

fn context_selection_claim(
    registry: &desk_diagnose_core::provider_registry::ProviderRegistry,
    inventory: &[CapabilityAvailability],
    readiness: Option<&ComputerUseReadiness>,
    selected_capability_ids: &[String],
    request_id: &str,
    actor_id: &str,
    device_id: &str,
    destination: &DestinationIdentity,
    now_unix_ms: u64,
) -> Result<crate::agent_session_store::ContextSelectionClaim, AgentError> {
    let selected = selected_capability_ids
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    for capability_id in &selected {
        if !inventory
            .iter()
            .any(|item| item.capability_id == *capability_id && item.ready)
        {
            return Err(transport_error(format!(
                "selected context capability is no longer ready: {capability_id}"
            )));
        }
    }

    let (incarnation, expires_at_unix_ms) = match readiness {
        Some(readiness) => {
            let expires_at = chrono::DateTime::parse_from_rfc3339(&readiness.expires_at)
                .map_err(|_| transport_error("invalid context readiness expiry"))?
                .timestamp_millis();
            let expires_at_unix_ms = u64::try_from(expires_at)
                .map_err(|_| transport_error("context readiness expiry predates Unix epoch"))?;
            if expires_at_unix_ms <= now_unix_ms {
                return Err(transport_error("selected context readiness expired"));
            }
            (
                readiness.interactive_session_incarnation.clone(),
                expires_at_unix_ms,
            )
        }
        None if selected.is_empty() => ("unavailable".to_string(), now_unix_ms + 1),
        None => return Err(transport_error("selected context Provider is unavailable")),
    };

    let runtime_bindings = inventory
        .iter()
        .filter(|item| item.ready)
        .map(|item| AttachmentRuntimeBinding {
            source_provider_id: item.provider_id.clone(),
            source_capability_id: item.capability_id.clone(),
            object_incarnation: incarnation.clone(),
        })
        .collect::<Vec<_>>();
    let mut candidates = Vec::new();
    for capability_id in selected_capability_ids {
        // CurrentScreen is a sensitive one-turn grant. It controls tool
        // exposure for this turn, but must never become durable session
        // attachment metadata.
        if capability_id == desk_diagnose_core::device_assistant::CURRENT_SCREEN_CAPABILITY_ID {
            continue;
        }
        let capability = registry.capability(capability_id).ok_or_else(|| {
            transport_error(format!(
                "unknown selected context capability: {capability_id}"
            ))
        })?;
        let provider = registry
            .provider_for_capability(capability_id)
            .ok_or_else(|| transport_error("selected context Provider is missing"))?;
        let attachment_id = format!("context-{}", uuid::Uuid::new_v4());
        let opaque_token = uuid::Uuid::new_v4().to_string();
        let observation_id = format!("selection-{}", uuid::Uuid::new_v4());
        let metadata = serde_json::to_vec(&serde_json::json!({
            "provider_id": provider.wire.provider_id,
            "capability_id": capability_id,
            "interactive_session_incarnation": incarnation,
        }))
        .map_err(|error| transport_error(format!("encode context metadata: {error}")))?;
        let digest = format!("{:x}", Sha256::digest(&metadata));
        let client_request_id = format!(
            "select-{:x}",
            Sha256::digest(format!("{request_id}:{capability_id}").as_bytes())
        );
        candidates.push(ContextAttachment {
            schema_version: CONTEXT_ATTACHMENT_SCHEMA_VERSION,
            attachment_id: attachment_id.clone(),
            client_request_id,
            actor_id: actor_id.to_string(),
            device_id: device_id.to_string(),
            surface: AgentSessionSurface::DeviceAssistant,
            // Today's selector binds the capability to the exact worker
            // incarnation. More specific Office/file/range selectors replace
            // this with their own immutable object kinds and incarnations.
            kind: ContextAttachmentKind::InteractiveSession,
            object_ref: AttachmentObjectRef {
                opaque_token: opaque_token.clone(),
                object_incarnation: incarnation.clone(),
                source_provider_id: provider.wire.provider_id.clone(),
                source_capability_id: capability_id.clone(),
            },
            bounds: AttachmentBounds {
                max_bytes: capability
                    .wire
                    .limits
                    .max_output_bytes
                    .min(MAX_ATTACHMENT_BYTES),
                max_objects: capability
                    .wire
                    .limits
                    .max_objects
                    .min(MAX_ATTACHMENT_OBJECTS),
            },
            display_summary: format!("{capability_id} on the current interactive session"),
            created_at_unix_ms: now_unix_ms,
            expires_at_unix_ms,
            envelope: DataEnvelope {
                schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
                envelope_id: format!("envelope-{attachment_id}"),
                content: ContentRef::EphemeralObservation {
                    observation_id,
                    size_bytes: u64::try_from(metadata.len()).unwrap_or(u64::MAX),
                    expires_at_unix_ms,
                },
                provenance: DataProvenance {
                    source_provider_id: provider.wire.provider_id.clone(),
                    source_tool_name: capability.wire.tool_name.clone(),
                    source_object_id: Some(opaque_token),
                    source_envelope_ids: Vec::new(),
                },
                digest_sha256: digest,
                sensitivity: Sensitivity::UserContent,
                allowed_destinations: vec![destination.clone()],
                retention: RetentionBoundary {
                    expires_at_unix_ms: Some(expires_at_unix_ms),
                    delete_with_run: false,
                },
            },
            state: AttachmentState::Active,
        });
    }
    Ok(crate::agent_session_store::ContextSelectionClaim {
        selected_capability_ids: selected_capability_ids.to_vec(),
        runtime_bindings,
        candidates,
        now_unix_ms,
    })
}

fn transport_error(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::TransportError,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

async fn send_frame(conn: &ConnectionState, frame: &SignalingModel) -> Result<(), String> {
    let text = serde_json::to_string(frame).map_err(|e| format!("encode frame: {e}"))?;
    tokio::time::timeout(SEND_TIMEOUT, async {
        conn.session.write().await.text(text).await
    })
    .await
    .map_err(|_| format!("send to {} timed out", conn.model.connection_id))?
    .map_err(|e| format!("send to {}: {e}", conn.model.connection_id))
}

async fn stream_event(
    connections: &SharedConnectionMap,
    browser_connection_id: &str,
    event: &DeviceAssistantEvent,
) {
    let browser = {
        let map = connections.read().await;
        map.get(browser_connection_id).cloned()
    };
    let Some(browser) = browser else {
        return;
    };
    let frame = SignalingModel::new(
        &event.request_id,
        SignalingType::DeviceAssistantUpdated,
        None,
        Some(browser_connection_id.to_string()),
        serde_json::to_value(event).ok(),
        None,
    );
    if let Err(e) = send_frame(&browser, &frame).await {
        log::warn!(
            "[device-assistant] failed to stream request_id={} kind={:?}: {e}",
            event.request_id,
            event.kind
        );
    }
}

pub async fn send_capability_inventory(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    browser_connection_id: String,
    target_connection_id: String,
) {
    let model_capabilities = crate::model_provider::load(&db)
        .await
        .map(|config| ModelCapabilities {
            image_input: config.supports_image_input,
        })
        .unwrap_or_default();
    let (registry, inventory, generated_at_unix_ms, _) = current_capability_projection(
        connections.as_ref(),
        &target_connection_id,
        model_capabilities,
    )
    .await;
    let snapshot = inventory_snapshot(
        &registry,
        desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner,
        generated_at_unix_ms,
        &inventory,
    )
    .expect("static Provider inventory projection must be valid");
    let browser = {
        let map = connections.read().await;
        map.get(&browser_connection_id).cloned()
    };
    let Some(browser) = browser else {
        return;
    };
    let frame = SignalingModel::new(
        &request_id,
        SignalingType::DeviceAssistantCapabilitiesUpdated,
        None,
        Some(browser_connection_id),
        serde_json::to_value(snapshot).ok(),
        None,
    );
    if let Err(error) = send_frame(&browser, &frame).await {
        log::warn!("[device-assistant] failed to send capability inventory: {error}");
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn update_context(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    browser_connection_id: String,
    target_connection_id: String,
    actor_user_id: i32,
    target_device_id: String,
    update: DeviceAssistantContextUpdate,
) {
    let result = async {
        let config = crate::model_provider::load(&db).await.map_err(|error| {
            transport_error(format!("failed to load model provider config: {error}"))
        })?;
        let destination = config.destination_identity().map_err(|error| {
            transport_error(format!("failed to resolve model destination: {error}"))
        })?;
        let (registry, inventory, now_unix_ms, readiness) =
            current_capability_projection(
                connections.as_ref(),
                &target_connection_id,
                ModelCapabilities {
                    image_input: config.supports_image_input,
                },
            )
            .await;
        if update.selected_capability_ids.iter().any(|capability_id| {
            capability_id
                == desk_diagnose_core::device_assistant::CURRENT_SCREEN_CAPABILITY_ID
        }) {
            return Err(transport_error(
                "CurrentScreen is a one-turn sensitive selection and cannot be saved as durable context",
            ));
        }
        let actor_id = actor_user_id.to_string();
        let selection = context_selection_claim(
            &registry,
            &inventory,
            readiness.as_ref(),
            &update.selected_capability_ids,
            &update.client_request_id,
            &actor_id,
            &target_device_id,
            &destination,
            now_unix_ms,
        )?;
        let conversation_key = derive_conversation_key(
            &actor_id,
            &target_device_id,
            Some(&update.conversation_id),
            &request_id,
        );
        let scope = AgentScope {
            granted: desk_diagnose_core::device_assistant::selected_context_capabilities(
                &update.selected_capability_ids,
            )
            .map_err(transport_error)?,
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: Some("oss-device-assistant-read-only".into()),
        };
        let store = crate::agent_session_store::SignalAgentSessionStore::new(db)
            .with_client_metadata(
                Some(update.conversation_id.clone()),
                AgentSessionSurface::DeviceAssistant,
            )
            .with_context_selection(selection);
        store
            .update_context_selection(
                &conversation_key,
                &actor_id,
                &target_device_id,
                scope,
                &chrono::Utc::now().to_rfc3339(),
            )
            .await
    }
    .await;

    let ack = DeviceAssistantContextUpdated {
        conversation_id: update.conversation_id,
        client_request_id: update.client_request_id,
        changed: result.as_ref().copied().unwrap_or(false),
        error: result.err().map(|error| error.message),
    };
    let browser = {
        let map = connections.read().await;
        map.get(&browser_connection_id).cloned()
    };
    let Some(browser) = browser else {
        return;
    };
    let frame = SignalingModel::new(
        &request_id,
        SignalingType::DeviceAssistantContextUpdated,
        None,
        Some(browser_connection_id),
        serde_json::to_value(ack).ok(),
        None,
    );
    if let Err(error) = send_frame(&browser, &frame).await {
        log::warn!("[device-assistant] failed to send context update: {error}");
    }
}

fn file_context_attachment(
    update: &DeviceAssistantObjectContextUpdate,
    object_ref: &desk_agent_protocol::computer_use::ObjectRef,
    display_summary: &str,
    actor_id: &str,
    device_id: &str,
    destination: &DestinationIdentity,
    now_unix_ms: u64,
) -> Result<ContextAttachment, AgentError> {
    let expires_at_unix_ms = u64::try_from(
        chrono::DateTime::parse_from_rfc3339(&object_ref.expires_at)
            .map_err(|_| transport_error("invalid file reference expiry"))?
            .timestamp_millis(),
    )
    .map_err(|_| transport_error("file reference expiry predates the Unix epoch"))?;
    if expires_at_unix_ms <= now_unix_ms {
        return Err(transport_error("selected file reference already expired"));
    }
    let encoded_ref = serde_json::to_string(object_ref)
        .map_err(|error| transport_error(format!("encode selected file reference: {error}")))?;
    let metadata = serde_json::to_vec(&serde_json::json!({
        "provider_id": desk_diagnose_core::device_assistant::FILE_WORKSPACE_PROVIDER_ID,
        "capability_id": desk_diagnose_core::device_assistant::FILE_METADATA_CAPABILITY_ID,
        "object_ref": object_ref,
        "display_summary": display_summary,
    }))
    .map_err(|error| transport_error(format!("encode selected file metadata: {error}")))?;
    let digest = format!("{:x}", Sha256::digest(&metadata));
    let attachment_id = format!("context-{}", uuid::Uuid::new_v4());
    Ok(ContextAttachment {
        schema_version: CONTEXT_ATTACHMENT_SCHEMA_VERSION,
        attachment_id: attachment_id.clone(),
        client_request_id: update.client_request_id.clone(),
        actor_id: actor_id.to_string(),
        device_id: device_id.to_string(),
        surface: AgentSessionSurface::DeviceAssistant,
        kind: match object_ref.object_kind {
            desk_agent_protocol::computer_use::ObjectKind::File => ContextAttachmentKind::File,
            desk_agent_protocol::computer_use::ObjectKind::Directory => {
                ContextAttachmentKind::DirectorySelection
            }
            _ => {
                return Err(transport_error(
                    "selected object is not a file or directory",
                ));
            }
        },
        object_ref: AttachmentObjectRef {
            opaque_token: encoded_ref,
            object_incarnation: format!("{}:{}", object_ref.snapshot_id, object_ref.token),
            source_provider_id: desk_diagnose_core::device_assistant::FILE_WORKSPACE_PROVIDER_ID
                .into(),
            source_capability_id: desk_diagnose_core::device_assistant::FILE_METADATA_CAPABILITY_ID
                .into(),
        },
        bounds: AttachmentBounds {
            max_bytes: 64 * 1024,
            max_objects: 32,
        },
        display_summary: display_summary.to_string(),
        created_at_unix_ms: now_unix_ms,
        expires_at_unix_ms,
        envelope: DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: format!("envelope-{attachment_id}"),
            content: ContentRef::EphemeralObservation {
                observation_id: format!("selection-{}", uuid::Uuid::new_v4()),
                size_bytes: metadata.len() as u64,
                expires_at_unix_ms,
            },
            provenance: DataProvenance {
                source_provider_id:
                    desk_diagnose_core::device_assistant::FILE_WORKSPACE_PROVIDER_ID.into(),
                source_tool_name: "inspect_selected_file_metadata".into(),
                source_object_id: Some(object_ref.token.clone()),
                source_envelope_ids: Vec::new(),
            },
            digest_sha256: digest,
            sensitivity: Sensitivity::Sensitive,
            allowed_destinations: vec![destination.clone()],
            retention: RetentionBoundary {
                expires_at_unix_ms: Some(expires_at_unix_ms),
                delete_with_run: false,
            },
        },
        state: AttachmentState::Active,
    })
}

fn terminal_context_attachment(
    update: &DeviceAssistantObjectContextUpdate,
    object_ref: &desk_agent_protocol::computer_use::ObjectRef,
    display_summary: &str,
    actor_id: &str,
    device_id: &str,
    destination: &DestinationIdentity,
    now_unix_ms: u64,
) -> Result<ContextAttachment, AgentError> {
    if object_ref.object_kind != desk_agent_protocol::computer_use::ObjectKind::TerminalOutput {
        return Err(transport_error(
            "selected object is not a terminal output snapshot",
        ));
    }
    let expires_at_unix_ms = u64::try_from(
        chrono::DateTime::parse_from_rfc3339(&object_ref.expires_at)
            .map_err(|_| transport_error("invalid terminal reference expiry"))?
            .timestamp_millis(),
    )
    .map_err(|_| transport_error("terminal reference expiry predates the Unix epoch"))?;
    if expires_at_unix_ms <= now_unix_ms {
        return Err(transport_error(
            "selected terminal reference already expired",
        ));
    }
    let encoded_ref = serde_json::to_string(object_ref)
        .map_err(|error| transport_error(format!("encode terminal reference: {error}")))?;
    let metadata = serde_json::to_vec(&serde_json::json!({
        "provider_id": desk_diagnose_core::device_assistant::TERMINAL_OUTPUT_PROVIDER_ID,
        "capability_id": desk_diagnose_core::device_assistant::TERMINAL_OUTPUT_CAPABILITY_ID,
        "object_ref": object_ref,
        "display_summary": display_summary,
    }))
    .map_err(|error| transport_error(format!("encode terminal metadata: {error}")))?;
    let digest = format!("{:x}", Sha256::digest(&metadata));
    let attachment_id = format!("context-{}", uuid::Uuid::new_v4());
    Ok(ContextAttachment {
        schema_version: CONTEXT_ATTACHMENT_SCHEMA_VERSION,
        attachment_id: attachment_id.clone(),
        client_request_id: update.client_request_id.clone(),
        actor_id: actor_id.to_string(),
        device_id: device_id.to_string(),
        surface: AgentSessionSurface::DeviceAssistant,
        kind: ContextAttachmentKind::TerminalSessionRef,
        object_ref: AttachmentObjectRef {
            opaque_token: encoded_ref,
            object_incarnation: format!("{}:{}", object_ref.snapshot_id, object_ref.token),
            source_provider_id: desk_diagnose_core::device_assistant::TERMINAL_OUTPUT_PROVIDER_ID
                .into(),
            source_capability_id:
                desk_diagnose_core::device_assistant::TERMINAL_OUTPUT_CAPABILITY_ID.into(),
        },
        bounds: AttachmentBounds {
            max_bytes: 32 * 1024,
            max_objects: 8,
        },
        display_summary: display_summary.to_string(),
        created_at_unix_ms: now_unix_ms,
        expires_at_unix_ms,
        envelope: DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: format!("envelope-{attachment_id}"),
            content: ContentRef::EphemeralObservation {
                observation_id: format!("terminal-selection-{}", uuid::Uuid::new_v4()),
                size_bytes: metadata.len() as u64,
                expires_at_unix_ms,
            },
            provenance: DataProvenance {
                source_provider_id:
                    desk_diagnose_core::device_assistant::TERMINAL_OUTPUT_PROVIDER_ID.into(),
                source_tool_name: "inspect_selected_terminal_output".into(),
                source_object_id: Some(object_ref.token.clone()),
                source_envelope_ids: Vec::new(),
            },
            digest_sha256: digest,
            sensitivity: Sensitivity::Sensitive,
            allowed_destinations: vec![destination.clone()],
            retention: RetentionBoundary {
                expires_at_unix_ms: Some(expires_at_unix_ms),
                delete_with_run: false,
            },
        },
        state: AttachmentState::Active,
    })
}

#[allow(clippy::too_many_arguments)]
pub async fn update_object_context(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    browser_connection_id: String,
    _target_connection_id: String,
    actor_user_id: i32,
    target_device_id: String,
    update: DeviceAssistantObjectContextUpdate,
) {
    let result = async {
        let config = crate::model_provider::load(&db).await.map_err(|error| {
            transport_error(format!("failed to load model provider config: {error}"))
        })?;
        let destination = config.destination_identity().map_err(|error| {
            transport_error(format!("failed to resolve model destination: {error}"))
        })?;
        let actor_id = actor_user_id.to_string();
        let now = chrono::Utc::now();
        let now_unix_ms = u64::try_from(now.timestamp_millis())
            .map_err(|_| transport_error("system clock predates the Unix epoch"))?;
        let mutation = match &update.operation {
            DeviceAssistantObjectContextOperation::AttachFile {
                object_ref,
                display_summary,
            } => {
                crate::agent_session_store::ObjectContextMutation::Attach(file_context_attachment(
                    &update,
                    object_ref,
                    display_summary,
                    &actor_id,
                    &target_device_id,
                    &destination,
                    now_unix_ms,
                )?)
            }
            DeviceAssistantObjectContextOperation::AttachTerminalOutput {
                object_ref,
                display_summary,
            } => crate::agent_session_store::ObjectContextMutation::Attach(
                terminal_context_attachment(
                    &update,
                    object_ref,
                    display_summary,
                    &actor_id,
                    &target_device_id,
                    &destination,
                    now_unix_ms,
                )?,
            ),
            DeviceAssistantObjectContextOperation::Detach { attachment_id } => {
                crate::agent_session_store::ObjectContextMutation::Detach {
                    attachment_id: attachment_id.clone(),
                }
            }
            DeviceAssistantObjectContextOperation::RefreshFile {
                stale_attachment_id,
                object_ref,
                display_summary,
            } => crate::agent_session_store::ObjectContextMutation::Refresh {
                stale_attachment_id: stale_attachment_id.clone(),
                replacement: file_context_attachment(
                    &update,
                    object_ref,
                    display_summary,
                    &actor_id,
                    &target_device_id,
                    &destination,
                    now_unix_ms,
                )?,
            },
        };
        let conversation_key = derive_conversation_key(
            &actor_id,
            &target_device_id,
            Some(&update.conversation_id),
            &request_id,
        );
        let scope = AgentScope {
            granted: vec![
                desk_agent_protocol::Capability::FileMetadataRead,
                desk_agent_protocol::Capability::FileContentRead,
                desk_agent_protocol::Capability::SpreadsheetFileInspect,
                desk_agent_protocol::Capability::SpreadsheetMergePreview,
                desk_agent_protocol::Capability::TerminalOutputRead,
            ],
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: Some("oss-device-assistant-read-only".into()),
        };
        crate::agent_session_store::SignalAgentSessionStore::new(db)
            .with_client_metadata(
                Some(update.conversation_id.clone()),
                AgentSessionSurface::DeviceAssistant,
            )
            .update_object_context(
                &conversation_key,
                &actor_id,
                &target_device_id,
                scope,
                &mutation,
                &now.to_rfc3339(),
            )
            .await
    }
    .await;

    let ack = DeviceAssistantObjectContextUpdated {
        conversation_id: update.conversation_id,
        client_request_id: update.client_request_id,
        changed: result.as_ref().copied().unwrap_or(false),
        error: result.err().map(|error| error.message),
    };
    let browser = {
        let map = connections.read().await;
        map.get(&browser_connection_id).cloned()
    };
    let Some(browser) = browser else {
        return;
    };
    let frame = SignalingModel::new(
        &request_id,
        SignalingType::DeviceAssistantObjectContextUpdated,
        None,
        Some(browser_connection_id),
        serde_json::to_value(ack).ok(),
        None,
    );
    if let Err(error) = send_frame(&browser, &frame).await {
        log::warn!("[device-assistant] failed to send object context update: {error}");
    }
}

struct SignalStoreHeartbeat {
    store: crate::agent_session_store::SignalAgentSessionStore,
}

impl LeaseHeartbeat for SignalStoreHeartbeat {
    fn start(&self, conversation_id: String, lease_token: u64) -> Box<dyn HeartbeatGuard> {
        let store = self.store.clone();
        let handle = actix_web::rt::spawn(async move {
            loop {
                tokio::time::sleep(HEARTBEAT_INTERVAL).await;
                if store
                    .heartbeat(
                        &conversation_id,
                        lease_token,
                        &chrono::Utc::now().to_rfc3339(),
                    )
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        Box::new(SignalHeartbeatGuard(handle))
    }
}

struct SignalHeartbeatGuard(actix_web::rt::task::JoinHandle<()>);
impl HeartbeatGuard for SignalHeartbeatGuard {}
impl Drop for SignalHeartbeatGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct MeteredModel {
    inner: SignalModelSeam,
    db: DatabaseConnection,
    model_name: String,
    destination: DestinationIdentity,
    selected_source_tools: std::collections::BTreeSet<String>,
    export_authorization_id: String,
    permission_resume: bool,
    model_call_ordinal: std::sync::atomic::AtomicU64,
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

use desk_diagnose_core::model_message_labels::model_bound_user_message;
#[allow(clippy::too_many_arguments)]
pub async fn run_turn(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    browser_connection_id: String,
    target_connection_id: String,
    actor_user_id: i32,
    target_device_id: String,
    ask: DeviceAssistantAsk,
) {
    run_turn_inner(
        connections,
        db,
        request_id,
        browser_connection_id,
        target_connection_id,
        actor_user_id,
        target_device_id,
        ask,
        None,
    )
    .await;
}

/// Resume the exact persisted requirement after its owner records a permission
/// decision. A hidden, server-authored protocol bridge is appended so
/// chat-completions providers reliably start a new turn; it is not recorded as
/// user input and carries no new requirement.
#[allow(clippy::too_many_arguments)]
pub async fn resume_after_permission_decision(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    target_connection_id: String,
    actor_user_id: i32,
    target_device_id: String,
    conversation_id: String,
    ask: DeviceAssistantAsk,
) {
    run_turn_inner(
        connections,
        db,
        request_id,
        String::new(),
        target_connection_id,
        actor_user_id,
        target_device_id,
        ask,
        Some(conversation_id),
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn run_turn_inner(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    browser_connection_id: String,
    target_connection_id: String,
    actor_user_id: i32,
    target_device_id: String,
    ask: DeviceAssistantAsk,
    resume_conversation_id: Option<String>,
) {
    stream_event(
        connections.as_ref(),
        &browser_connection_id,
        &AgentEvent::status(&request_id, 0, "accepting"),
    )
    .await;

    let config = match crate::model_provider::load(&db).await {
        Ok(config) => config,
        Err(e) => {
            stream_event(
                connections.as_ref(),
                &browser_connection_id,
                &AgentEvent::error(
                    &request_id,
                    1,
                    transport_error(format!("failed to load model provider config: {e}")),
                ),
            )
            .await;
            return;
        }
    };
    let seam = match SignalModelSeam::from_config(&config) {
        Ok(seam) => seam,
        Err(error) => {
            stream_event(
                connections.as_ref(),
                &browser_connection_id,
                &AgentEvent::error(&request_id, 1, error),
            )
            .await;
            return;
        }
    };
    let destination = match config.destination_identity() {
        Ok(destination) => destination,
        Err(error) => {
            stream_event(
                connections.as_ref(),
                &browser_connection_id,
                &AgentEvent::error(
                    &request_id,
                    1,
                    transport_error(format!("failed to resolve model destination: {error}")),
                ),
            )
            .await;
            return;
        }
    };
    let actor_id = actor_user_id.to_string();
    let client_conversation_id = ask
        .conversation_id
        .as_deref()
        .map(str::trim)
        .filter(|id| is_valid_client_conversation_id(id))
        .map(str::to_string);
    let conversation_id = resume_conversation_id.clone().unwrap_or_else(|| {
        derive_conversation_key(
            &actor_id,
            &target_device_id,
            ask.conversation_id.as_deref(),
            &request_id,
        )
    });
    let now_unix_ms = u64::try_from(chrono::Utc::now().timestamp_millis())
        .expect("current time is after the Unix epoch");
    let session_reader = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .with_client_metadata(
            client_conversation_id.clone(),
            AgentSessionSurface::DeviceAssistant,
        );
    let snapshot = match session_reader
        .read_snapshot_for_subject(&conversation_id, &actor_id, &target_device_id)
        .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            stream_event(
                connections.as_ref(),
                &browser_connection_id,
                &AgentEvent::error(&request_id, 1, error),
            )
            .await;
            return;
        }
    };
    let mut selected_file_roots = Vec::new();
    let mut selected_spreadsheet_roots = Vec::new();
    let mut selected_terminal_roots = Vec::new();
    if !ask.selected_attachment_ids.is_empty() {
        let Some(snapshot) = snapshot.as_ref() else {
            stream_event(
                connections.as_ref(),
                &browser_connection_id,
                &AgentEvent::error(
                    &request_id,
                    1,
                    transport_error("selected Device Assistant attachment does not exist"),
                ),
            )
            .await;
            return;
        };
        for attachment_id in &ask.selected_attachment_ids {
            let Some(attachment) = snapshot.context_attachments.iter().find(|attachment| {
                attachment.attachment_id == *attachment_id && attachment.is_active_at(now_unix_ms)
            }) else {
                stream_event(
                    connections.as_ref(),
                    &browser_connection_id,
                    &AgentEvent::error(
                        &request_id,
                        1,
                        transport_error(format!(
                            "selected Device Assistant attachment is stale or missing: {attachment_id}"
                        )),
                    ),
                )
                .await;
                return;
            };
            if matches!(
                attachment.kind,
                ContextAttachmentKind::File | ContextAttachmentKind::DirectorySelection
            ) {
                let object_ref = match serde_json::from_str::<
                    desk_agent_protocol::computer_use::ObjectRef,
                >(&attachment.object_ref.opaque_token)
                {
                    Ok(object_ref) => object_ref,
                    Err(_) => {
                        stream_event(
                            connections.as_ref(),
                            &browser_connection_id,
                            &AgentEvent::error(
                                &request_id,
                                1,
                                transport_error("selected file attachment is invalid"),
                            ),
                        )
                        .await;
                        return;
                    }
                };
                if is_spreadsheet_input_attachment(attachment.kind, &attachment.display_summary) {
                    selected_spreadsheet_roots.push(object_ref.clone());
                }
                selected_file_roots.push(object_ref);
            } else if attachment.kind == ContextAttachmentKind::TerminalSessionRef {
                let object_ref = match serde_json::from_str::<
                    desk_agent_protocol::computer_use::ObjectRef,
                >(&attachment.object_ref.opaque_token)
                {
                    Ok(object_ref) => object_ref,
                    Err(_) => {
                        stream_event(
                            connections.as_ref(),
                            &browser_connection_id,
                            &AgentEvent::error(
                                &request_id,
                                1,
                                transport_error("selected terminal attachment is invalid"),
                            ),
                        )
                        .await;
                        return;
                    }
                };
                selected_terminal_roots.push(object_ref);
            }
        }
    }
    let model_name = config.model.clone().unwrap_or_default();
    let (provider_registry, inventory, _, readiness) = current_capability_projection(
        connections.as_ref(),
        &target_connection_id,
        ModelCapabilities {
            image_input: config.supports_image_input,
        },
    )
    .await;
    let readiness_revision = readiness
        .as_ref()
        .map(|readiness| readiness.revision)
        .unwrap_or(1);
    let office_selected = ask.selected_capability_ids.iter().any(|capability_id| {
        capability_id == desk_diagnose_core::device_assistant::OFFICE_DOCUMENT_CAPABILITY_ID
    });
    let selected_office_document = readiness.as_ref().and_then(|readiness| {
        readiness
            .context_references
            .iter()
            .find(|reference| {
                reference.capability == desk_agent_protocol::Capability::OfficeDocumentInspect
            })
            .map(|reference| reference.object_ref.clone())
    });
    let selected_live_spreadsheet = readiness.as_ref().and_then(|readiness| {
        readiness
            .context_references
            .iter()
            .find(|reference| {
                reference.capability == desk_agent_protocol::Capability::SpreadsheetLiveInspect
            })
            .map(|reference| reference.object_ref.clone())
    });
    let selected_live_document = readiness.as_ref().and_then(|readiness| {
        readiness
            .context_references
            .iter()
            .find(|reference| {
                reference.capability == desk_agent_protocol::Capability::DocumentLiveInspect
            })
            .map(|reference| reference.object_ref.clone())
    });
    let selected_live_presentation = readiness.as_ref().and_then(|readiness| {
        readiness
            .context_references
            .iter()
            .find(|reference| {
                reference.capability == desk_agent_protocol::Capability::PresentationLiveInspect
            })
            .map(|reference| reference.object_ref.clone())
    });
    let selected_browser_surface = readiness.as_ref().and_then(|readiness| {
        readiness
            .context_references
            .iter()
            .find(|reference| {
                reference.capability == desk_agent_protocol::Capability::BrowserPageObserve
            })
            .map(|reference| reference.object_ref.clone())
    });
    let selected_outlook_surface = readiness.as_ref().and_then(|readiness| {
        readiness
            .context_references
            .iter()
            .find(|reference| {
                reference.capability
                    == desk_agent_protocol::Capability::CommunicationOutlookNewHandoffConfirmed
            })
            .map(|reference| reference.object_ref.clone())
    });
    if office_selected && selected_office_document.is_none() {
        stream_event(
            connections.as_ref(),
            &browser_connection_id,
            &AgentEvent::error(
                &request_id,
                1,
                transport_error(
                    "the selected Excel document is no longer available; refresh context before sending",
                ),
            ),
        )
        .await;
        return;
    }
    let capability_grants =
        match crate::capability_grant_store::SignalCapabilityGrantStore::new(db.clone())
            .list_for_subject(&conversation_id, &actor_id, &target_device_id)
            .await
        {
            Ok(grants) => grants,
            Err(error) => {
                stream_event(
                    connections.as_ref(),
                    &browser_connection_id,
                    &AgentEvent::error(
                        &request_id,
                        1,
                        transport_error(format!("failed to load capability grants: {error}")),
                    ),
                )
                .await;
                return;
            }
        };
    let resume_desktop_ui_inspect = resume_conversation_id.is_some()
        && has_active_resume_desktop_ui_inspect_grant(&capability_grants, now_unix_ms);
    let mut selected_source_tools = ask
        .selected_capability_ids
        .iter()
        .filter_map(|capability_id| provider_registry.capability(capability_id))
        .map(|capability| capability.wire.tool_name.clone())
        .collect::<std::collections::BTreeSet<_>>();
    if resume_desktop_ui_inspect {
        // This is the same server-triggered continuation of the owner's
        // original requirement. The explicit read grant remains the authority;
        // restoring the source tool here only keeps its read-back envelope
        // eligible for the same model destination after the one-turn context
        // attachment itself has expired.
        selected_source_tools.insert("inspect_desktop_ui".into());
    }
    if !selected_file_roots.is_empty() {
        selected_source_tools.insert("inspect_selected_file_metadata".into());
        let selected_file_count = selected_file_roots
            .iter()
            .filter(|object_ref| {
                object_ref.object_kind == desk_agent_protocol::computer_use::ObjectKind::File
            })
            .count();
        let has_output_directory = selected_file_roots.iter().any(|object_ref| {
            object_ref.object_kind == desk_agent_protocol::computer_use::ObjectKind::Directory
        });
        if selected_file_count > 0 {
            selected_source_tools.insert("read_selected_text_file".into());
        }
        if selected_file_count == 1 {
            selected_source_tools.extend(
                [
                    "inspect_selected_numbers_with_iwork",
                    "inspect_selected_pages_with_iwork",
                    "inspect_selected_keynote_with_iwork",
                ]
                .into_iter()
                .map(str::to_string),
            );
            if has_output_directory {
                selected_source_tools.extend(
                    [
                        "patch_selected_numbers_copy",
                        "replace_selected_pages_copy_body",
                        "patch_selected_keynote_copy",
                    ]
                    .into_iter()
                    .map(str::to_string),
                );
            }
        }
        if has_output_directory {
            selected_source_tools.insert("create_text_artifact_in_selected_directory".into());
            selected_source_tools.insert("create_local_communication_draft".into());
        }
    }
    if !selected_spreadsheet_roots.is_empty() {
        selected_source_tools.insert("inspect_selected_spreadsheets".into());
        selected_source_tools.insert("preview_spreadsheet_merge".into());
        if selected_file_roots.iter().any(|object_ref| {
            object_ref.object_kind == desk_agent_protocol::computer_use::ObjectKind::Directory
        }) {
            selected_source_tools.insert("create_workbook_from_merge_preview".into());
            selected_source_tools.insert("create_formula_workbook_from_merge_preview".into());
            selected_source_tools.insert("create_word_report_from_merge_preview".into());
        }
    }
    if !selected_terminal_roots.is_empty() {
        selected_source_tools.insert("inspect_selected_terminal_output".into());
    }
    if selected_browser_surface.is_some() {
        selected_source_tools.extend(
            [
                "browser_open_page",
                "browser_navigate_page",
                "browser_take_snapshot",
                "browser_wait_for",
                "browser_fill_form",
                "browser_activate_element",
                "prepare_gmail_web_draft_handoff",
                "prepare_slack_web_message_handoff",
            ]
            .into_iter()
            .map(str::to_string),
        );
    }
    if selected_outlook_surface.is_some() {
        selected_source_tools.insert("prepare_outlook_new_draft_handoff".into());
    }
    if selected_live_spreadsheet.is_some() {
        selected_source_tools.insert("inspect_live_spreadsheet".into());
        selected_source_tools.insert("patch_live_spreadsheet_cell".into());
    }
    if selected_live_document.is_some() {
        selected_source_tools.insert("inspect_live_document".into());
        selected_source_tools.insert("replace_live_document_body".into());
    }
    if selected_live_presentation.is_some() {
        selected_source_tools.insert("inspect_live_presentation".into());
        selected_source_tools.insert("patch_live_presentation_slide".into());
    }
    selected_source_tools
        .insert(desk_diagnose_core::device_assistant::PREVIEW_COMPUTER_ACTION_TOOL.to_string());
    selected_source_tools.insert(crate::web_research::WEB_FETCH_TOOL_NAME.to_string());
    selected_source_tools.insert(crate::web_research::WEB_SEARCH_TOOL_NAME.to_string());
    selected_source_tools.extend(
        desk_diagnose_core::device_assistant::SYSTEM_DIAGNOSTIC_TOOL_NAMES
            .into_iter()
            .map(str::to_string),
    );
    selected_source_tools.insert("execute_confirmed_command".into());
    selected_source_tools
        .insert(desk_diagnose_core::device_assistant::EXECUTE_CONFIRMED_UI_ACTION_TOOL.into());
    selected_source_tools
        .insert(desk_diagnose_core::device_assistant::EXECUTE_CONFIRMED_RAW_INPUT_TOOL.into());
    let export_authorization_id = format!(
        "assistant-export-{:x}",
        Sha256::digest(format!("{actor_user_id}:{target_device_id}:{request_id}").as_bytes())
    );
    let model = MeteredModel {
        inner: seam,
        db: db.clone(),
        model_name: model_name.clone(),
        destination: destination.clone(),
        selected_source_tools,
        export_authorization_id,
        permission_resume: resume_conversation_id.is_some(),
        model_call_ordinal: std::sync::atomic::AtomicU64::new(0),
    };
    let callable = callable_tools(&provider_registry, &inventory)
        .expect("static Provider registry must produce complete capability availability");
    let mut registry = filter_model_compatible_tools(
        &callable,
        ModelCapabilities {
            image_input: config.supports_image_input,
        },
    );
    let mut selected_tool_capability_ids = ask.selected_capability_ids.clone();
    if resume_desktop_ui_inspect {
        selected_tool_capability_ids
            .push(desk_diagnose_core::device_assistant::DESKTOP_UI_CAPABILITY_ID.into());
    }
    selected_tool_capability_ids
        .push(desk_diagnose_core::device_assistant::WEB_RESEARCH_FETCH_CAPABILITY_ID.into());
    selected_tool_capability_ids.extend(
        desk_diagnose_core::device_assistant::SYSTEM_DIAGNOSTIC_CAPABILITY_IDS
            .into_iter()
            .map(str::to_string),
    );
    selected_tool_capability_ids
        .push(desk_diagnose_core::device_assistant::SYSTEM_COMMAND_CAPABILITY_ID.into());
    if !selected_file_roots.is_empty() {
        selected_tool_capability_ids
            .push(desk_diagnose_core::device_assistant::FILE_METADATA_CAPABILITY_ID.into());
        let selected_file_count = selected_file_roots
            .iter()
            .filter(|object_ref| {
                object_ref.object_kind == desk_agent_protocol::computer_use::ObjectKind::File
            })
            .count();
        let has_output_directory = selected_file_roots.iter().any(|object_ref| {
            object_ref.object_kind == desk_agent_protocol::computer_use::ObjectKind::Directory
        });
        if selected_file_count > 0 {
            selected_tool_capability_ids
                .push(desk_diagnose_core::device_assistant::FILE_CONTENT_CAPABILITY_ID.into());
        }
        if selected_file_count == 1 {
            selected_tool_capability_ids.extend(
                [
                    desk_diagnose_core::device_assistant::SPREADSHEET_BATCH_INSPECT_CAPABILITY_ID,
                    desk_diagnose_core::device_assistant::DOCUMENT_BATCH_INSPECT_CAPABILITY_ID,
                    desk_diagnose_core::device_assistant::PRESENTATION_BATCH_INSPECT_CAPABILITY_ID,
                ]
                .into_iter()
                .map(str::to_string),
            );
            if has_output_directory {
                selected_tool_capability_ids.extend(
                    [
                        desk_diagnose_core::device_assistant::SPREADSHEET_BATCH_PATCH_CAPABILITY_ID,
                        desk_diagnose_core::device_assistant::DOCUMENT_BATCH_PATCH_CAPABILITY_ID,
                        desk_diagnose_core::device_assistant::PRESENTATION_BATCH_PATCH_CAPABILITY_ID,
                    ]
                    .into_iter()
                    .map(str::to_string),
                );
            }
        }
        if has_output_directory {
            selected_tool_capability_ids.push(
                desk_diagnose_core::device_assistant::FILE_ARTIFACT_CREATE_CAPABILITY_ID.into(),
            );
            selected_tool_capability_ids.push(
                desk_diagnose_core::device_assistant::LOCAL_COMMUNICATION_DRAFT_CREATE_CAPABILITY_ID
                    .into(),
            );
        }
    }
    if !selected_spreadsheet_roots.is_empty() {
        selected_tool_capability_ids
            .push(desk_diagnose_core::device_assistant::SPREADSHEET_FILE_CAPABILITY_ID.into());
        selected_tool_capability_ids
            .push(desk_diagnose_core::device_assistant::SPREADSHEET_MERGE_CAPABILITY_ID.into());
        if selected_file_roots.iter().any(|object_ref| {
            object_ref.object_kind == desk_agent_protocol::computer_use::ObjectKind::Directory
        }) {
            selected_tool_capability_ids.push(
                desk_diagnose_core::device_assistant::SPREADSHEET_WORKBOOK_CREATE_CAPABILITY_ID
                    .into(),
            );
            selected_tool_capability_ids.push(
                desk_diagnose_core::device_assistant::SPREADSHEET_FORMULA_WORKBOOK_CREATE_CAPABILITY_ID
                    .into(),
            );
            selected_tool_capability_ids.push(
                desk_diagnose_core::device_assistant::WORD_DOCUMENT_CREATE_CAPABILITY_ID.into(),
            );
        }
    }
    if !selected_terminal_roots.is_empty() {
        selected_tool_capability_ids
            .push(desk_diagnose_core::device_assistant::TERMINAL_OUTPUT_CAPABILITY_ID.into());
    }
    if selected_browser_surface.is_some() {
        selected_tool_capability_ids.extend(
            [
                desk_diagnose_core::device_assistant::BROWSER_OPEN_CAPABILITY_ID,
                desk_diagnose_core::device_assistant::BROWSER_NAVIGATE_CAPABILITY_ID,
                desk_diagnose_core::device_assistant::BROWSER_SNAPSHOT_CAPABILITY_ID,
                desk_diagnose_core::device_assistant::BROWSER_WAIT_CAPABILITY_ID,
                desk_diagnose_core::device_assistant::BROWSER_FILL_CAPABILITY_ID,
                desk_diagnose_core::device_assistant::BROWSER_ACTIVATE_CAPABILITY_ID,
                desk_diagnose_core::device_assistant::GMAIL_WEB_HANDOFF_CAPABILITY_ID,
                desk_diagnose_core::device_assistant::SLACK_WEB_HANDOFF_CAPABILITY_ID,
            ]
            .into_iter()
            .map(str::to_string),
        );
    }
    if selected_outlook_surface.is_some() {
        selected_tool_capability_ids
            .push(desk_diagnose_core::device_assistant::OUTLOOK_NEW_HANDOFF_CAPABILITY_ID.into());
    }
    if selected_live_spreadsheet.is_some() {
        selected_tool_capability_ids.extend(
            [
                desk_diagnose_core::device_assistant::SPREADSHEET_LIVE_INSPECT_CAPABILITY_ID,
                desk_diagnose_core::device_assistant::SPREADSHEET_LIVE_PATCH_CAPABILITY_ID,
            ]
            .into_iter()
            .map(str::to_string),
        );
    }
    if selected_live_document.is_some() {
        selected_tool_capability_ids.extend(
            [
                desk_diagnose_core::device_assistant::DOCUMENT_LIVE_INSPECT_CAPABILITY_ID,
                desk_diagnose_core::device_assistant::DOCUMENT_LIVE_PATCH_CAPABILITY_ID,
            ]
            .into_iter()
            .map(str::to_string),
        );
    }
    if selected_live_presentation.is_some() {
        selected_tool_capability_ids.extend(
            [
                desk_diagnose_core::device_assistant::PRESENTATION_LIVE_INSPECT_CAPABILITY_ID,
                desk_diagnose_core::device_assistant::PRESENTATION_LIVE_PATCH_CAPABILITY_ID,
            ]
            .into_iter()
            .map(str::to_string),
        );
    }
    desk_diagnose_core::device_assistant::retain_selected_context_tools(
        &provider_registry,
        &mut registry,
        &selected_tool_capability_ids,
    );
    let capability_catalog = desk_diagnose_core::permission_tools::discoverable_catalog_prompt(
        &provider_registry,
        &inventory,
        &registry,
    );
    let permission_requests = snapshot
        .as_ref()
        .map(|snapshot| snapshot.permission_requests.as_slice())
        .unwrap_or_default();
    let capability_authorization =
        desk_diagnose_core::permission_tools::capability_authorization_prompt(
            &capability_grants,
            permission_requests,
            now_unix_ms,
            readiness_revision,
        );
    let permission_continuation_exact_tools =
        desk_diagnose_core::permission_tools::active_exact_authorized_tool_names(
            &capability_grants,
            permission_requests,
            now_unix_ms,
            readiness_revision,
        );
    // Internal run projection is not a Provider capability and grants no device
    // authority. It is always callable so the model can keep the user-visible
    // task assessment current even when no device context was selected.
    registry.extend(desk_diagnose_core::task_status_tools::task_status_tool_registry());
    // Permission planning is also internal run control. It can only create a
    // normalized pending request; it never widens this callable registry.
    registry.extend(desk_diagnose_core::permission_tools::permission_planning_tool_registry());
    let context_selection = match context_selection_claim(
        &provider_registry,
        &inventory,
        readiness.as_ref(),
        &ask.selected_capability_ids,
        &request_id,
        &actor_id,
        &target_device_id,
        &destination,
        now_unix_ms,
    ) {
        Ok(selection) => selection,
        Err(error) => {
            stream_event(
                connections.as_ref(),
                &browser_connection_id,
                &AgentEvent::error(&request_id, 1, error),
            )
            .await;
            return;
        }
    };
    let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .with_client_metadata(
            client_conversation_id.clone(),
            AgentSessionSurface::DeviceAssistant,
        )
        .with_context_selection(context_selection);
    let heartbeat = SignalStoreHeartbeat {
        store: sessions.clone(),
    };
    let mut granted = desk_diagnose_core::device_assistant::selected_context_capabilities(
        &ask.selected_capability_ids,
    )
    .expect("control authorizer validated selected Device Assistant context");
    if resume_desktop_ui_inspect
        && !granted.contains(&desk_agent_protocol::Capability::DesktopUiInspect)
    {
        granted.push(desk_agent_protocol::Capability::DesktopUiInspect);
    }
    extend_fixed_exact_action_capabilities(&registry, &mut granted);
    granted.extend(desk_diagnose_core::device_assistant::system_diagnostic_capabilities());
    // The command Provider itself remains a fixed R3 one-shot exact grant.
    // These two edge capabilities only let the daemon accept the server-owned
    // classifier's read-only vs mutating effect after it reproduces the sealed
    // safe-template plan; neither one exposes the legacy free-form exec tool.
    granted.push(desk_agent_protocol::Capability::ShellExecReadonly);
    granted.push(desk_agent_protocol::Capability::ShellExecConfirmed);
    if !selected_file_roots.is_empty() {
        granted.push(desk_agent_protocol::Capability::FileMetadataRead);
        if selected_file_roots.iter().any(|object_ref| {
            object_ref.object_kind == desk_agent_protocol::computer_use::ObjectKind::File
        }) {
            granted.push(desk_agent_protocol::Capability::FileContentRead);
        }
        if selected_file_roots.iter().any(|object_ref| {
            object_ref.object_kind == desk_agent_protocol::computer_use::ObjectKind::Directory
        }) {
            granted.push(desk_agent_protocol::Capability::FileArtifactCreateConfirmed);
            granted.push(desk_agent_protocol::Capability::CommunicationLocalDraftCreateConfirmed);
        }
    }
    if !selected_spreadsheet_roots.is_empty() {
        granted.push(desk_agent_protocol::Capability::SpreadsheetFileInspect);
        granted.push(desk_agent_protocol::Capability::SpreadsheetMergePreview);
        if selected_file_roots.iter().any(|object_ref| {
            object_ref.object_kind == desk_agent_protocol::computer_use::ObjectKind::Directory
        }) {
            granted.push(desk_agent_protocol::Capability::SpreadsheetWorkbookCreateConfirmed);
            granted
                .push(desk_agent_protocol::Capability::SpreadsheetFormulaWorkbookCreateConfirmed);
            granted.push(desk_agent_protocol::Capability::WordDocumentCreateConfirmed);
        }
    }
    if !selected_terminal_roots.is_empty() {
        granted.push(desk_agent_protocol::Capability::TerminalOutputRead);
    }
    if selected_browser_surface.is_some() {
        granted.extend([
            desk_agent_protocol::Capability::BrowserPageObserve,
            desk_agent_protocol::Capability::BrowserPageNavigateConfirmed,
            desk_agent_protocol::Capability::BrowserInputFallbackConfirmed,
            desk_agent_protocol::Capability::BrowserExternalDraftWriteConfirmed,
        ]);
    }
    if selected_outlook_surface.is_some() {
        granted.push(desk_agent_protocol::Capability::CommunicationOutlookNewHandoffConfirmed);
    }
    if selected_live_spreadsheet.is_some() {
        granted.extend([
            desk_agent_protocol::Capability::SpreadsheetLiveInspect,
            desk_agent_protocol::Capability::SpreadsheetLivePatchConfirmed,
        ]);
    }
    if selected_live_document.is_some() {
        granted.extend([
            desk_agent_protocol::Capability::DocumentLiveInspect,
            desk_agent_protocol::Capability::DocumentLivePatchConfirmed,
        ]);
    }
    if selected_live_presentation.is_some() {
        granted.extend([
            desk_agent_protocol::Capability::PresentationLiveInspect,
            desk_agent_protocol::Capability::PresentationLivePatchConfirmed,
        ]);
    }
    let mutation_enabled = granted.iter().any(capability_enables_mutation);
    let scope = AgentScope {
        granted,
        // Even a provider configured for confirmed exec is hard-clamped here.
        mode: if mutation_enabled {
            config
                .execution_mode
                .restrict_to(ExecutionMode::ConfirmEachAction)
        } else {
            ExecutionMode::ReadOnly
        },
        expires_at: None,
        policy_name: Some("oss-device-assistant-provider".into()),
    };
    let clock = || chrono::Utc::now().to_rfc3339();
    let turn_id = uuid::Uuid::new_v4().to_string();
    let (available_exec_shells, max_command_runtime_ms) = {
        let connections = connections.read().await;
        connections
            .get(&target_connection_id)
            .map(|target| {
                (
                    target.model.version_info.available_exec_shell_list(),
                    target
                        .model
                        .version_info
                        .max_ai_command_runtime_ms
                        .unwrap_or(desk_agent_protocol::exec_policy::DEFAULT_TIMEOUT_MS),
                )
            })
            .unwrap_or_default()
    };
    let tools = crate::remote_tool_edge::SignalDeviceAssistantTools::new(
        db.clone(),
        provider_registry.clone(),
        connections.clone().into_inner(),
        crate::remote_tool_edge::global_remote_tool_pending(),
        target_connection_id,
        target_device_id.clone(),
        actor_id.clone(),
        config.wire_protocol.map(|value| format!("{value:?}")),
        Some(model_name.clone()),
        selected_office_document,
        selected_live_spreadsheet,
        selected_live_document,
        selected_live_presentation,
        selected_file_roots,
        selected_spreadsheet_roots,
        selected_terminal_roots,
        selected_browser_surface,
        selected_outlook_surface,
        ask.question.clone(),
        conversation_id.clone(),
        turn_id.clone(),
        OSS_ASSISTANT_POLICY_REVISION,
        readiness_revision,
        available_exec_shells,
        max_command_runtime_ms,
    );
    let mut system_prompt = build_device_assistant_system_message_with_catalog(
        ask.locale.as_deref(),
        &format!("{capability_catalog}\n\n{}", capability_authorization.text),
    );
    if resume_conversation_id.is_some() {
        system_prompt.text.push_str(
            "\n\nPERMISSION DECISION RESUME (server authoritative): the owner has just decided the pending permission request. Re-read CURRENT AUTHORIZED GRANTS above. Do not request or ask for the same permission again. If a matching active grant exists, continue the existing user requirement now and call the authorized tool. If the item was denied or narrowed so the call no longer matches, adapt the plan or explain the remaining blocker. This trigger adds no new user requirement and does not change the original tool inputs.",
        );
    }
    if let Some(expires_at_unix_ms) =
        capability_authorization.approved_exact_input_expires_at_unix_ms
    {
        system_prompt = match bind_exact_authorization_system_message(
            system_prompt,
            destination.clone(),
            expires_at_unix_ms,
        ) {
            Ok(system_prompt) => system_prompt,
            Err(error) => {
                stream_event(
                    connections.as_ref(),
                    &browser_connection_id,
                    &AgentEvent::error(&request_id, 1, error),
                )
                .await;
                return;
            }
        };
    }
    let deps = LoopDeps {
        session_seam: &sessions,
        model: &model,
        tools: &tools,
        content_safety: desk_diagnose_core::content_safety::ContentSafetyMode::Disabled,
        registry: &registry,
        provider_registry: Some(&provider_registry),
        capability_inventory: Some(&inventory),
        permission_continuation_exact_tools: &permission_continuation_exact_tools,
        response_format: ResponseFormatSpec::None,
        system_prompt,
        max_steps_per_turn: config.max_steps_per_turn,
        max_same_tool_per_turn: config.max_same_tool_calls_per_turn,
        clock: &clock,
        heartbeat: Some(&heartbeat),
    };
    let accepted_at = clock();
    if resume_conversation_id.is_some() {
        let decision_message = match model_bound_permission_resume_message(
            format!("{request_id}-decision"),
            destination,
            &ask.question,
        ) {
            Ok(message) => message,
            Err(error) => {
                log::warn!("[device-assistant] failed to bind permission resume event: {error:?}");
                return;
            }
        };
        let claim = ClaimTurnParams {
            conversation_id,
            actor_id,
            device_id: target_device_id,
            policy_revision: OSS_ASSISTANT_POLICY_REVISION,
            current_pdp_scope: scope,
            turn_id: turn_id.clone(),
            request_id: None,
            connection_id: None,
            trigger_origin: TriggerOrigin::PermissionDecision,
            now: accepted_at,
        };
        let mut sink =
            StreamingTurnSink::starting_at(|_event: DeviceAssistantEvent| {}, request_id, 0);
        sink.set_provenance(AiProvenance::stamp(
            config.model,
            Some(chrono::Utc::now().to_rfc3339()),
        ));
        sink.turn_started(&turn_id);
        match resume_agent_turn_after_permission(&deps, claim, decision_message, &mut sink).await {
            Ok(LoopOutcome::TurnBusy) => {
                // A newer owner follow-up won the claim. Its input supersedes
                // this grant-triggered resume, so no retry is appropriate.
            }
            Ok(outcome) => sink.finish_outcome(&outcome),
            Err(error) => {
                log::warn!(
                    "[device-assistant] permission-resumed turn failed: kind={:?}, retryable={}, message={}",
                    error.kind,
                    error.retryable,
                    error.message
                );
                sink.error(error);
            }
        }
        return;
    }
    let user =
        match model_bound_user_message(ask.client_message_id.clone(), ask.question, destination) {
            Ok(user) => user,
            Err(error) => {
                stream_event(
                    connections.as_ref(),
                    &browser_connection_id,
                    &AgentEvent::error(&request_id, 1, error),
                )
                .await;
                return;
            }
        };
    let ack = match crate::agent_run_event_store::SignalAgentRunEventStore::new(db.clone())
        .append_user_followup(crate::agent_run_event_store::AppendUserFollowupParams {
            event_id: ask.client_message_id,
            run_id: conversation_id.clone(),
            client_conversation_id: client_conversation_id.clone(),
            actor_id: actor_id.clone(),
            device_id: target_device_id.clone(),
            surface: AgentSessionSurface::DeviceAssistant,
            policy_revision: OSS_ASSISTANT_POLICY_REVISION,
            current_scope: scope.clone(),
            message: user.clone(),
            created_at: accepted_at.clone(),
        })
        .await
    {
        Ok(ack) => ack,
        Err(error) => {
            stream_event(
                connections.as_ref(),
                &browser_connection_id,
                &AgentEvent::error(&request_id, 1, error),
            )
            .await;
            return;
        }
    };
    stream_event(
        connections.as_ref(),
        &browser_connection_id,
        &AgentEvent::status(&request_id, 1, "accepted"),
    )
    .await;
    if ack.already_handled {
        match sessions.read_snapshot(&conversation_id).await {
            Ok(Some(snapshot)) if snapshot.handled_input_seq >= ack.input_seq => {
                if let Some(answer) = latest_committed_answer(&snapshot) {
                    stream_event(
                        connections.as_ref(),
                        &browser_connection_id,
                        &AgentEvent::answer(&request_id, 2, answer),
                    )
                    .await;
                    return;
                }
            }
            Ok(_) => {}
            Err(error) => {
                stream_event(
                    connections.as_ref(),
                    &browser_connection_id,
                    &AgentEvent::error(&request_id, 2, error),
                )
                .await;
                return;
            }
        }
    }
    let claim = ClaimTurnParams {
        conversation_id,
        actor_id,
        device_id: target_device_id,
        policy_revision: OSS_ASSISTANT_POLICY_REVISION,
        current_pdp_scope: scope,
        turn_id: turn_id.clone(),
        request_id: Some(request_id.clone()),
        connection_id: Some(browser_connection_id.clone()),
        trigger_origin: TriggerOrigin::User,
        now: accepted_at,
    };

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeviceAssistantEvent>();
    let forward_connections = connections.clone();
    let forward_browser = browser_connection_id.clone();
    let forwarder = actix_web::rt::spawn(async move {
        while let Some(event) = rx.recv().await {
            stream_event(forward_connections.as_ref(), &forward_browser, &event).await;
        }
    });
    let mut sink = StreamingTurnSink::starting_at(
        move |event: DeviceAssistantEvent| {
            let _ = tx.send(event);
        },
        request_id,
        2,
    );
    sink.set_provenance(AiProvenance::stamp(
        config.model,
        Some(chrono::Utc::now().to_rfc3339()),
    ));
    sink.turn_started(&turn_id);
    loop {
        match run_agent_turn(&deps, claim.clone(), user.clone(), &mut sink).await {
            Ok(LoopOutcome::TurnBusy) => {
                match sessions.read_snapshot(&claim.conversation_id).await {
                    Ok(Some(snapshot)) if snapshot.handled_input_seq >= ack.input_seq => {
                        if let Some(answer) = latest_committed_answer(&snapshot) {
                            sink.on_answer_committed(&answer);
                        } else {
                            sink.error(transport_error(
                                "the accepted Device Assistant input settled without an answer; send a follow-up to retry",
                            ));
                        }
                        break;
                    }
                    Ok(_) => {
                        // Another request owns the run. The durable follow-up is
                        // already ACKed, so wait until that owner either handles it
                        // or is superseded, then race to claim the newest revision.
                        tokio::time::sleep(BUSY_FOLLOWUP_RETRY_INTERVAL).await;
                    }
                    Err(error) => {
                        sink.error(error);
                        break;
                    }
                }
            }
            Ok(outcome) => {
                sink.finish_outcome(&outcome);
                break;
            }
            Err(error) => {
                log::warn!(
                    "[device-assistant] owner turn failed: kind={:?}, retryable={}, message={}",
                    error.kind,
                    error.retryable,
                    error.message
                );
                sink.error(error);
                break;
            }
        }
    }
    drop(sink);
    let _ = forwarder.await;
}

fn is_supported_spreadsheet_attachment(display_summary: &str) -> bool {
    let display_summary = display_summary.to_ascii_lowercase();
    [".xlsx", ".csv", ".tsv"]
        .iter()
        .any(|extension| display_summary.ends_with(extension))
}

fn is_spreadsheet_input_attachment(kind: ContextAttachmentKind, display_summary: &str) -> bool {
    kind == ContextAttachmentKind::DirectorySelection
        || (kind == ContextAttachmentKind::File
            && is_supported_spreadsheet_attachment(display_summary))
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::data_lineage::Sensitivity;
    use desk_diagnose_core::seam::NullTurnSink;
    use sea_orm::{Database, EntityTrait};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn spreadsheet_tools_accept_explicit_directories_and_supported_files_only() {
        assert!(is_spreadsheet_input_attachment(
            ContextAttachmentKind::DirectorySelection,
            "Quarterly reports"
        ));
        assert!(is_spreadsheet_input_attachment(
            ContextAttachmentKind::File,
            "report.XLSX"
        ));
        assert!(!is_spreadsheet_input_attachment(
            ContextAttachmentKind::File,
            "notes.txt"
        ));
    }

    #[test]
    fn permission_resume_bridge_replays_original_requirement_with_user_sensitivity() {
        let destination = DestinationIdentity::Model {
            connection_id: "gateway-1".into(),
            connection_revision: 2,
            model_id: "model-1".into(),
            profile_revision: 3,
        };
        let message = model_bound_permission_resume_message(
            "permission-resume-request-1-decision".into(),
            destination.clone(),
            "prepare the exact draft and do not send it",
        )
        .unwrap();

        assert_eq!(message.role, ChatRole::User);
        assert!(message.text.starts_with("AUTOMATIC SERVER CONTROL EVENT"));
        assert!(
            message
                .text
                .contains("prepare the exact draft and do not send it")
        );
        let envelope = message.data_envelope.unwrap();
        assert_eq!(envelope.sensitivity, Sensitivity::UserContent);
        assert_eq!(envelope.allowed_destinations, vec![destination]);
        assert_eq!(
            envelope.provenance.source_provider_id,
            "assistant-runtime-control"
        );
        assert!(envelope.retention.delete_with_run);
        envelope.validate().unwrap();
    }

    #[test]
    fn exact_ui_action_remains_in_scope_for_permission_resume() {
        let mut tools = desk_diagnose_core::device_assistant::device_assistant_tool_registry();
        desk_diagnose_core::device_assistant::retain_selected_context_tools(
            &desk_diagnose_core::device_assistant::device_assistant_provider_registry(),
            &mut tools,
            &[],
        );
        let mut granted =
            desk_diagnose_core::device_assistant::selected_context_capabilities(&[]).unwrap();

        extend_fixed_exact_action_capabilities(&tools, &mut granted);

        assert!(
            granted.contains(&desk_agent_protocol::Capability::DesktopUiActionConfirmed),
            "an exact approved action must remain callable after its observation attachment expires"
        );
        assert!(
            granted.contains(&desk_agent_protocol::Capability::DesktopInputFallbackConfirmed),
            "an exact approved raw-input fallback must remain callable after its observation attachment expires"
        );
        assert!(granted.iter().any(capability_enables_mutation));

        tools.retain(|tool| {
            !matches!(
                tool.name(),
                desk_diagnose_core::device_assistant::EXECUTE_CONFIRMED_UI_ACTION_TOOL
                    | desk_diagnose_core::device_assistant::EXECUTE_CONFIRMED_RAW_INPUT_TOOL
            )
        });
        let mut unavailable = Vec::new();
        extend_fixed_exact_action_capabilities(&tools, &mut unavailable);
        assert!(unavailable.is_empty());
    }

    #[test]
    fn permission_resume_recognizes_only_active_desktop_ui_read_grants() {
        use desk_agent_protocol::capability_grant::{
            CAPABILITY_GRANT_SCHEMA_VERSION, CapabilityGrant, CapabilityGrantIssuer,
            CapabilityGrantLimits, CapabilityGrantUsePolicy, CapabilityRiskTier,
        };
        use desk_agent_protocol::capability_provider::{CapabilityEffect, ProductSurface};

        let grant = CapabilityGrant {
            schema_version: CAPABILITY_GRANT_SCHEMA_VERSION,
            grant_id: "grant-ui-read".into(),
            actor_id: "owner".into(),
            run_id: "run".into(),
            surface: ProductSurface::OssPersonalOwner,
            target_device_id: "device".into(),
            target_session_id: None,
            provider_id: desk_diagnose_core::device_assistant::DESKTOP_UI_PROVIDER_ID.into(),
            capability_id: desk_diagnose_core::device_assistant::DESKTOP_UI_CAPABILITY_ID.into(),
            tool_name: "inspect_desktop_ui".into(),
            tool_schema_version: 1,
            effect: CapabilityEffect::ReadDevice,
            risk_tier: CapabilityRiskTier::R1,
            resource_scope: vec!["target:current_device".into()],
            operation_scope: vec!["observe".into()],
            export_destinations: Vec::new(),
            allowed_envelope_ids: Vec::new(),
            allowed_content_digests_sha256: Vec::new(),
            use_policy: CapabilityGrantUsePolicy::Reusable,
            canonical_input_digest_sha256: None,
            issued_by: CapabilityGrantIssuer::UserDecision,
            issued_at_unix_ms: 100,
            expires_at_unix_ms: 1_000,
            remaining_uses: 2,
            limits: CapabilityGrantLimits {
                max_bytes_per_call: 64 * 1024,
                max_items_per_call: 180,
                max_calls: 4,
            },
            policy_revision: 1,
            readiness_revision: 1,
            revoked_at_unix_ms: None,
            revoked_reason: None,
        };

        assert!(has_active_resume_desktop_ui_inspect_grant(
            std::slice::from_ref(&grant),
            500
        ));
        let mut exhausted = grant.clone();
        exhausted.remaining_uses = 0;
        assert!(!has_active_resume_desktop_ui_inspect_grant(
            &[exhausted],
            500
        ));
        assert!(!has_active_resume_desktop_ui_inspect_grant(&[grant], 1_000));
    }

    #[test]
    fn exact_authorization_projection_is_not_labeled_as_public_system_text() {
        let destination = DestinationIdentity::Model {
            connection_id: "gateway-1".into(),
            connection_revision: 2,
            model_id: "model-1".into(),
            profile_revision: 3,
        };
        let message = ChatMessage::text(
            "system-exact-authorization",
            ChatRole::System,
            r#"approved_exact_input={"body":"private draft"}"#,
        );
        let bound = bind_exact_authorization_system_message(
            message,
            destination.clone(),
            1_900_000_000_000,
        )
        .unwrap();

        let envelope = bound.data_envelope.unwrap();
        assert_eq!(envelope.sensitivity, Sensitivity::Sensitive);
        assert_eq!(envelope.allowed_destinations, vec![destination]);
        assert_eq!(
            envelope.retention.expires_at_unix_ms,
            Some(1_900_000_000_000)
        );
        assert!(envelope.retention.delete_with_run);
        assert_eq!(
            envelope.provenance.source_tool_name,
            "capability-authorization"
        );
        envelope.validate().unwrap();
    }

    async fn capture_one_openai_request_with_sse(
        listener: TcpListener,
        sse: &'static str,
    ) -> Vec<u8> {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let (header_end, content_length) = loop {
            let read = socket.read(&mut buffer).await.unwrap();
            assert!(
                read > 0,
                "fake gateway connection closed before request body"
            );
            request.extend_from_slice(&buffer[..read]);
            if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .expect("awc send_json must use a bounded Content-Length");
                break (header_end + 4, content_length);
            }
        };
        while request.len() < header_end + content_length {
            let read = socket.read(&mut buffer).await.unwrap();
            assert!(read > 0, "fake gateway connection closed mid-body");
            request.extend_from_slice(&buffer[..read]);
        }

        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
            sse.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
        request[header_end..header_end + content_length].to_vec()
    }

    async fn capture_one_openai_request(listener: TcpListener) -> Vec<u8> {
        capture_one_openai_request_with_sse(
            listener,
            concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"captured-ok\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n"
            ),
        )
        .await
    }

    fn selected_tool_envelope(content: &str) -> DataEnvelope {
        let digest_sha256 = format!("{:x}", Sha256::digest(content.as_bytes()));
        DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: "selected-tool-result".into(),
            content: ContentRef::ImmutableBlob {
                blob_id: "selected-tool-result-content".into(),
                sha256: digest_sha256.clone(),
                size_bytes: content.len() as u64,
                media_type: "application/json".into(),
            },
            provenance: DataProvenance {
                source_provider_id: "windows-uia".into(),
                source_tool_name: "inspect_desktop_ui".into(),
                source_object_id: Some("snapshot-1".into()),
                source_envelope_ids: Vec::new(),
            },
            digest_sha256,
            sensitivity: Sensitivity::Sensitive,
            // A read result has no implicit model destination. MeteredModel must
            // mint the exact ExportData projection before provider I/O.
            allowed_destinations: Vec::new(),
            retention: RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: true,
            },
        }
    }

    #[actix_web::test]
    async fn fake_gateway_captures_only_authorized_envelopes_and_persists_receipt() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let capture = actix_web::rt::spawn(capture_one_openai_request(listener));

        let config = crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("fake-model".into()),
            base_url: Some(format!("http://{address}")),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        };
        let destination = config.destination_identity().unwrap();
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let model = MeteredModel {
            inner: SignalModelSeam::from_config(&config).unwrap(),
            db: db.clone(),
            model_name: "fake-model".into(),
            destination: destination.clone(),
            selected_source_tools: ["inspect_desktop_ui".to_string()].into_iter().collect(),
            export_authorization_id: "fake-http-export".into(),
            permission_resume: false,
            model_call_ordinal: std::sync::atomic::AtomicU64::new(0),
        };

        let removed_attachment_marker = "removed-marker-is-not-attached";
        let user = model_bound_user_message(
            "user-message-1".into(),
            "inspect the selected context".into(),
            destination,
        )
        .unwrap();
        let selected_content = r#"{\"window\":\"Excel\",\"button\":\"Help\"}"#;
        let mut tool = ChatMessage::tool_result("tool-1", "call-1", selected_content);
        tool.data_envelope = Some(selected_tool_envelope(selected_content));
        let request = ModelRequest::text_only(
            vec![
                ChatMessage::text("system", ChatRole::System, "trusted system prompt"),
                user,
                tool,
            ],
            ResponseFormatSpec::None,
        );
        let mut sink = NullTurnSink;
        let turn = model.call(request, &mut sink).await.unwrap();
        assert_eq!(turn.text, "captured-ok");

        let body = capture.await.unwrap();
        let body = String::from_utf8(body).unwrap();
        assert!(body.contains("selected context"));
        assert!(body.contains("Excel"));
        assert!(!body.contains(removed_attachment_marker));
        assert!(!body.contains("data_envelope"));
        assert!(!body.contains("allowed_destinations"));
        assert!(!body.contains("test-only-key"));

        let receipts = crate::entity::model_egress_receipt::Entity::find()
            .all(&db)
            .await
            .unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(
            receipts[0].state,
            crate::model_egress_store::STATE_SUCCEEDED
        );
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&receipts[0].envelope_ids_json)
                .unwrap()
                .len(),
            3
        );
    }

    #[actix_web::test]
    async fn empty_gateway_end_turn_reaches_bounded_agent_loop_recovery() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let capture = actix_web::rt::spawn(capture_one_openai_request_with_sse(
            listener,
            concat!(
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":0}}\n\n",
                "data: [DONE]\n\n"
            ),
        ));
        let config = crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("fake-model".into()),
            base_url: Some(format!("http://{address}")),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        };
        let destination = config.destination_identity().unwrap();
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let model = MeteredModel {
            inner: SignalModelSeam::from_config(&config).unwrap(),
            db: db.clone(),
            model_name: "fake-model".into(),
            destination: destination.clone(),
            selected_source_tools: Default::default(),
            export_authorization_id: "empty-http-export".into(),
            permission_resume: false,
            model_call_ordinal: std::sync::atomic::AtomicU64::new(0),
        };
        let request = ModelRequest::text_only(
            vec![
                ChatMessage::text("system", ChatRole::System, "trusted system prompt"),
                model_bound_user_message(
                    "user-message-empty".into(),
                    "continue".into(),
                    destination,
                )
                .unwrap(),
            ],
            ResponseFormatSpec::None,
        );
        let turn = model.call(request, &mut NullTurnSink).await.unwrap();
        assert!(turn.text.is_empty());
        assert!(turn.tool_calls.is_empty());
        assert_eq!(
            turn.stop_reason,
            desk_diagnose_core::chat::StopReason::EndTurn
        );
        let _ = capture.await.unwrap();
        let receipts = crate::entity::model_egress_receipt::Entity::find()
            .all(&db)
            .await
            .unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].state, crate::model_egress_store::STATE_FAILED);
        assert!(receipts[0].model_output_envelope_id.is_none());
    }
}
