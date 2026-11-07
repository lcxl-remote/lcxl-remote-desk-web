//! Project only the current request's registered, unlocked device references.

use super::*;
use desk_diagnose_core::{
    action_turn_fence::AssistantTurnFence,
    request_devices::{RequestDevice, selected_read},
    session::PersistedAgentSession,
};

fn unavailable() -> AgentError {
    error(
        AgentErrorKind::PermissionDenied,
        "Selected assistant device is unavailable",
        false,
        true,
    )
}

pub(super) fn selected_read_connection(state: &ConnectionState) -> bool {
    !state.lifecycle.is_retired()
        && state
            .model
            .ai_assistant_settings
            .as_ref()
            .is_some_and(|settings| settings.enabled)
}

impl SignalAiAssistantTools {
    pub(super) async fn directory_for_request_device(
        &self,
        session: &PersistedAgentSession,
        target_ref: &str,
        path: &str,
    ) -> Result<
        (
            String,
            desk_agent_protocol::computer_use::FileDirectoryResolveOutput,
        ),
        AgentError,
    > {
        let (held, connection) = self.selected_connection(session, target_ref).await?;
        if !crate::computer_use_readiness::global_computer_use_readiness_cache()
            .get_fresh(&connection, chrono::Utc::now())
            .is_some_and(|current| {
                current.readiness.capabilities.iter().any(|item| {
                    item.capability == desk_agent_protocol::Capability::FileMetadataRead
                        && item.supported
                        && item.ready
                })
            })
        {
            return Err(unavailable());
        }
        let resolved = super::directory::resolve_candidate(
            &self.connections,
            &connection,
            &self.actor_id,
            &held.device_id,
            path,
        )
        .await?;
        Ok((held.device_id, resolved))
    }

    pub(super) async fn selected_connection(
        &self,
        session: &PersistedAgentSession,
        reference: &str,
    ) -> Result<(AssistantTurnFence, String), AgentError> {
        if !self.target_device_id.is_empty()
            || !session.has_admitted_personal_input()
            || session.actor_id != self.actor_id
            || session.conversation_id != self.run_id
            || session.current_turn_id.as_deref() != Some(self.turn_id.as_str())
        {
            return Err(unavailable());
        }
        crate::issue_connections::check_work_on(
            &self.db,
            session,
            true,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .map_err(|error| error.into_agent_error())?;
        let target = session
            .assistant_request_device(reference)
            .ok_or_else(unavailable)?;
        let held = AssistantTurnFence::for_device(session, &target.device_id)
            .map_err(|_| unavailable())?
            .ok_or_else(unavailable)?;
        if !desk_assistant_data_store::device_roster::permits_current_target(
            &self.db,
            session,
            &held.device_id,
        )
        .await
        .map_err(|error| error.into_agent_error())?
        {
            return Err(unavailable());
        }
        let connection_id = self
            .connections
            .read()
            .await
            .current_oss_server(&target.device_id)
            .filter(|state| selected_read_connection(state))
            .map(|state| state.model.connection_id.clone())
            .ok_or_else(|| {
                error(
                    AgentErrorKind::TargetOffline,
                    "Selected assistant device is offline or disabled",
                    true,
                    true,
                )
            })?;
        Ok((held, connection_id))
    }

    pub(super) async fn command_policy_for_request_device(
        &self,
        session: &PersistedAgentSession,
        target_ref: &str,
    ) -> Result<desk_diagnose_core::command_confirmation::CommandPolicyContext, AgentError> {
        let (held, connection_id) = self.selected_connection(session, target_ref).await?;
        if crate::agent_run_event_store::eligible_request_device_ids(
            &self.db,
            std::slice::from_ref(&held.device_id),
        )
        .await?
        .is_empty()
        {
            return Err(unavailable());
        }
        let readiness = crate::computer_use_readiness::global_computer_use_readiness_cache()
            .get_fresh(&connection_id, chrono::Utc::now())
            .ok_or_else(unavailable)?;
        if !readiness.readiness.capabilities.iter().any(|item| {
            item.capability == desk_agent_protocol::Capability::ShellExecConfirmed
                && item.supported
                && item.ready
        }) {
            return Err(unavailable());
        }
        let policy = crate::command_policy::current(
            &self.db,
            self.connections.as_ref(),
            &connection_id,
            &self.actor_id,
        )
        .await?;
        if policy.target_device_id != held.device_id {
            return Err(unavailable());
        }
        Ok(policy)
    }

    pub(super) async fn read_request_device(
        &self,
        session: &PersistedAgentSession,
        call: &ToolCall,
    ) -> Result<ReadOutcome, AgentError> {
        let (reference, native_call) = selected_read(call)?;
        if !self
            .original_input
            .get()
            .is_some_and(|input| input.selection.is_selectable_input())
        {
            return Err(unavailable());
        }
        let (held, connection_id) = self.selected_connection(session, &reference).await?;
        let readiness_revision =
            crate::computer_use_readiness::global_computer_use_readiness_cache()
                .get_fresh(&connection_id, chrono::Utc::now())
                .map(|current| current.readiness.revision)
                .ok_or_else(unavailable)?;
        desk_diagnose_core::request_devices::require_current_read_target(
            session,
            &native_call,
            &desk_diagnose_core::request_devices::SelectedDeviceTarget {
                device_id: held.device_id.clone(),
                target_ref: reference,
                connection_id: connection_id.clone(),
                readiness_revision,
            },
        )?;

        let scoped =
            self.scoped_request_device(held, connection_id, readiness_revision, Vec::new(), 0);
        scoped.observation.set(&native_call, self.observed(call));
        let result = scoped.read_outcome(&native_call).await;
        let (output, ok) = match result {
            Ok(ReadOutcome::Completed {
                output,
                ok,
                event_id: None,
                data_envelope: None,
                background_task: None,
            }) => (output, ok),
            Err(error) => (
                desk_diagnose_core::model_input::error_output(&call.name, &error),
                false,
            ),
            Ok(_) => return Err(unavailable()),
        };
        let data_envelope = scoped.read_data_envelope(&native_call, &output)?;
        Ok(ReadOutcome::Completed {
            output,
            ok,
            event_id: None,
            data_envelope,
            background_task: None,
        })
    }

    pub(super) async fn execute_request_device_command(
        &self,
        call: &ToolCall,
        ctx: &ExecContext,
        policy: &desk_diagnose_core::command_confirmation::CommandPolicyContext,
    ) -> Result<ExecOutcome, AgentError> {
        let held = ctx
            .assistant_turn_fence
            .as_ref()
            .filter(|held| held.device_id == policy.target_device_id)
            .ok_or_else(unavailable)?;
        let connection_id = self
            .connections
            .read()
            .await
            .current_oss_server(&held.device_id)
            .filter(|state| selected_read_connection(state))
            .map(|state| state.model.connection_id.clone())
            .ok_or_else(unavailable)?;
        let readiness = crate::computer_use_readiness::global_computer_use_readiness_cache()
            .get_fresh(&connection_id, chrono::Utc::now())
            .ok_or_else(unavailable)?;
        let scoped = self.scoped_request_device(
            held.clone(),
            connection_id,
            readiness.readiness.revision,
            policy.available_shells.clone(),
            policy.max_runtime_ms,
        );
        scoped.observe_tool_input(call, self.observed(call));
        scoped.authorize_and_execute_command(call, ctx).await
    }

    pub(super) async fn execute_selected_action(
        &self,
        call: &ToolCall,
        ctx: &ExecContext,
    ) -> Result<ExecOutcome, AgentError> {
        let (reference, native) = desk_diagnose_core::request_devices::split_device_call(call)?;
        let session = self.authoritative_session().await?;
        let (held, connection) = self.selected_connection(&session, &reference).await?;
        if ctx.assistant_turn_fence.as_ref() != Some(&held) {
            return Err(unavailable());
        }
        let current = crate::computer_use_readiness::global_computer_use_readiness_cache()
            .get_fresh(&connection, chrono::Utc::now())
            .ok_or_else(unavailable)?;
        let browser =
            desk_diagnose_core::ai_assistant::browser_surface_context(Some(&current.readiness));
        let mut scoped =
            self.scoped_request_device(held, connection, current.readiness.revision, Vec::new(), 0);
        let state = Arc::get_mut(&mut scoped.0).expect("configure selected browser before sharing");
        state.selected_browser_surface = browser;
        scoped.observe_tool_input(&native, self.observed(call));
        scoped.confirm_and_exec(&native, ctx).await
    }

    fn scoped_request_device(
        &self,
        held: AssistantTurnFence,
        connection_id: String,
        readiness_revision: u64,
        available_exec_shells: Vec<String>,
        max_command_runtime_ms: u32,
    ) -> Self {
        let mut scoped = Self::new(
            self.db.clone(),
            self.provider_registry.clone(),
            self.connections.clone(),
            self.pending.clone(),
            connection_id,
            held.device_id.clone(),
            self.actor_id.clone(),
            self.model_provider.clone(),
            self.model_name.clone(),
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
            None,
            self.current_user_message.clone(),
            self.run_id.clone(),
            self.turn_id.clone(),
            self.policy_revision,
            readiness_revision,
            available_exec_shells,
            max_command_runtime_ms,
        )
        .with_model_egress_policy(self.model_egress_policy.clone());
        let state = Arc::get_mut(&mut scoped.0).expect("configure selected device before sharing");
        state.selected_device_fence = Some(held);
        if let Some(original) = self.original_input.get() {
            state.original_input = OnceLock::from(original.clone());
        }
        state.timeout = self.timeout;
        scoped
    }

    pub(super) async fn current_request_devices(
        &self,
        session: &PersistedAgentSession,
    ) -> Result<Vec<RequestDevice>, AgentError> {
        if !session.has_admitted_personal_input()
            || session.actor_id != self.actor_id
            || session.actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string()
            || session.conversation_id != self.run_id
        {
            return Err(unavailable());
        }
        match crate::issue_connections::check_work_on(
            &self.db,
            session,
            true,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        {
            Ok(()) => {}
            Err(desk_assistant_data_store::AssistantStoreError::Inaccessible) => return Ok(vec![]),
            Err(error) => return Err(error.into_agent_error()),
        }
        let selected =
            desk_assistant_data_store::device_roster::current_request_devices(&self.db, session)
                .await
                .map_err(|error| error.into_agent_error())?;
        let mut devices = request_device_catalogue(&self.db, &self.connections, &selected).await?;
        let store = SignalCapabilityGrantStore::new(self.db.clone());
        let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
        for device in &mut devices {
            if let Some(target) = &device.read_target
                && session
                    .permission_requests
                    .iter()
                    .any(|request| request.selected_device_target() == Some(target))
            {
                let grants = desk_diagnose_core::future::boxed(|| {
                    store.list_for_subject(&self.run_id, &self.actor_id, &target.device_id)
                })
                .await
                .map_err(|_| unavailable())?;
                device.approved_read_tools =
                    desk_diagnose_core::request_devices::approved_read_tools(
                        session, target, &grants, now,
                    );
            }
        }
        Ok(devices)
    }
}

pub(crate) async fn request_device_catalogue(
    db: &DatabaseConnection,
    connections: &SharedConnectionMap,
    selected: &[String],
) -> Result<Vec<RequestDevice>, AgentError> {
    let eligible = crate::agent_run_event_store::eligible_request_device_ids(db, selected).await?;
    let connections = connections.read().await;
    let now = chrono::Utc::now();
    let readiness = crate::computer_use_readiness::global_computer_use_readiness_cache();
    Ok(selected
        .iter()
        .filter(|id| eligible.contains(*id))
        .filter_map(|id| {
            let connection = connections
                .current_oss_server(id)
                .filter(|state| !state.lifecycle.is_retired());
            if connection.is_some_and(|state| {
                !state
                    .model
                    .ai_assistant_settings
                    .as_ref()
                    .is_some_and(|settings| settings.enabled)
            }) {
                return None;
            }
            let current =
                connection.and_then(|state| readiness.get_fresh(&state.model.connection_id, now));
            let ready_capabilities = current
                .as_ref()
                .into_iter()
                .flat_map(|current| &current.readiness.capabilities)
                .filter(|item| item.supported && item.ready)
                .map(|item| item.capability)
                .collect();
            Some(RequestDevice {
                native_readiness: current.as_ref().map(|record| record.readiness.clone()),
                read_target: connection.zip(current.as_ref()).map(|(state, current)| {
                    desk_diagnose_core::request_devices::SelectedDeviceTarget {
                        device_id: id.clone(),
                        target_ref: id.clone(),
                        connection_id: state.model.connection_id.clone(),
                        readiness_revision: current.readiness.revision,
                    }
                }),
                ready_provider_ids: current
                    .as_ref()
                    .and_then(|current| {
                        desk_diagnose_core::request_devices::ready_provider_ids(&current.readiness)
                            .ok()
                    })
                    .unwrap_or_default(),
                approved_read_tools: Vec::new(),
                target_ref: id.clone(),
                name: connection.and_then(|state| state.model.peer.display_name.clone()),
                platform: connection.and_then(|state| {
                    use desk_signal_facade::model::os::OperationSystemEnum::*;
                    match state.model.peer.operation_system {
                        Windows => Some("Windows"),
                        Linux => Some("Linux"),
                        Mac => Some("macOS"),
                        Android => Some("Android"),
                        Ios => Some("iOS"),
                        _ => None,
                    }
                }),
                online: connection.is_some(),
                ready_capabilities,
            })
        })
        .collect())
}
