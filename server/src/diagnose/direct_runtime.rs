//! Direct-runtime entry that drives the agentic loop for an inbound `Diagnose`.
//!
//! This is the production wiring that turns a control-end `Diagnose` request into
//! an agentic tool-calling turn on the daemon-local (Default / DeskServer)
//! runtime, replacing the single-turn collect→model→render path. It assembles the
//! three Direct seams ([`super::agent`]) into a long-lived [`DirectAgentRuntime`]
//! and runs one turn per request, streaming progress to the control end as
//! `DiagnoseEvent` frames via the shared [`StreamingTurnSink`] bridge.
//!
//! Scope: the read tools the model may call are gated by the device's local
//! collection policy (`allow_logs`) and, when the request rode a manager
//! authorization, intersected with the PDP-granted scope. Only read tools are
//! exposed here; mutating execution lands with the exec path.
//!
//! A request id keys a fresh in-memory session (one single-turn conversation per
//! request); cross-question continuation over a stable conversation id is a later
//! step that needs a protocol field for it.

use std::sync::Arc;

use desk_agent_protocol::authz::AuthorizationBlock;
use desk_agent_protocol::diagnose::DiagnoseRequestData;
use desk_agent_protocol::{AgentScope, Capability, ExecutionMode};
use desk_diagnose_core::agent_loop::{LoopDeps, run_agent_turn};
use desk_diagnose_core::agentic_prompt::build_agentic_system_message;
use desk_diagnose_core::chat::{ChatMessage, ChatRole};
use desk_diagnose_core::prompt::ResponseFormatSpec;
use desk_diagnose_core::read_tools::read_tool_registry;
use desk_diagnose_core::registry::RegisteredTool;
use desk_diagnose_core::seam::{ClaimTurnParams, ModelSeam, SessionSeam, ToolSeam};
use desk_diagnose_core::stream::{DiagnoseFrameSink, StreamingTurnSink};

use super::agent::{AdapterModelSeam, DirectToolSeam, InMemorySessionSeam};
use super::model::ProviderAdapterSelector;
use super::redaction::RegexRedactor;
use crate::model::settings::SharedSettings;
use crate::worker::agent::LocalDeviceAgent;

/// The actor id stamped on a Direct agentic session when no manager authorization
/// resolved a user identity (single-machine / remote-signaling links).
const DIRECT_AGENT_ACTOR: &str = "local-operator";

/// Current time as an RFC3339 string.
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// The long-lived Direct agentic runtime: the read-only seams + tool registry the
/// `Diagnose` entry runs the loop over. Built once per daemon (Default /
/// DeskServer) alongside the orchestrator; absent where no in-process worker can
/// read locally (ServiceDaemon), where `Diagnose` replies feature-unavailable.
///
/// The seams are held as `Send + Sync` trait objects so the runtime can be shared
/// in the router context across the proxy's tasks, and so tests can inject a
/// scripted model. The model call's future is still `!Send` (it runs on actix's
/// single-threaded runtime); only the seam objects need to be shareable.
#[derive(Clone)]
pub struct DirectAgentRuntime {
    tools: Arc<dyn ToolSeam + Send + Sync>,
    model: Arc<dyn ModelSeam + Send + Sync>,
    session: Arc<dyn SessionSeam + Send + Sync>,
    registry: Arc<Vec<RegisteredTool>>,
    settings: Arc<SharedSettings>,
}

impl DirectAgentRuntime {
    /// Build the production runtime over the in-process device agent: read tools
    /// against it (redacted fail-closed), the provider adapter as the model, and
    /// an in-memory session store.
    pub fn new(agent: Arc<LocalDeviceAgent>, settings: Arc<SharedSettings>) -> Self {
        let tools = Arc::new(DirectToolSeam::new(
            agent,
            Arc::new(RegexRedactor::new()),
            DIRECT_AGENT_ACTOR,
        ));
        let model = Arc::new(AdapterModelSeam::new(
            Arc::new(ProviderAdapterSelector),
            settings.clone(),
        ));
        Self {
            tools,
            model,
            session: Arc::new(InMemorySessionSeam::new()),
            registry: Arc::new(read_tool_registry()),
            settings,
        }
    }

    /// Inject the seams directly. Used by tests to drive the loop with a scripted
    /// model; production builds via [`new`](DirectAgentRuntime::new).
    pub fn with_seams(
        model: Arc<dyn ModelSeam + Send + Sync>,
        tools: Arc<dyn ToolSeam + Send + Sync>,
        session: Arc<dyn SessionSeam + Send + Sync>,
        settings: Arc<SharedSettings>,
    ) -> Self {
        Self {
            tools,
            model,
            session,
            registry: Arc::new(read_tool_registry()),
            settings,
        }
    }

    /// Run one agentic diagnosis turn, streaming `DiagnoseEvent` frames to `sink`.
    /// Computes the read scope from the local policy + the optional manager
    /// authorization, then drives the loop to a terminal frame.
    pub async fn run<S: DiagnoseFrameSink>(
        &self,
        request_id: &str,
        request: DiagnoseRequestData,
        authz: Option<&AuthorizationBlock>,
        sink: S,
    ) {
        let allow_logs = self.settings.read().await.collection_policy.allow_logs;
        let scope = direct_read_scope(&self.registry, allow_logs, authz);
        let (actor_id, device_id, tenant_id) = subject_for(authz);
        run_direct_agent_turn(
            self.session.as_ref(),
            self.model.as_ref(),
            self.tools.as_ref(),
            &self.registry,
            request_id,
            request,
            scope,
            actor_id,
            device_id,
            tenant_id,
            sink,
        )
        .await;
    }
}

/// The read scope a Direct agentic turn runs under: the read tools' capabilities
/// allowed by the local collection policy (`log.recent` gated by `allow_logs`),
/// intersected with the manager-granted scope when the request rode a PDP
/// authorization. Always read-only.
fn direct_read_scope(
    registry: &[RegisteredTool],
    allow_logs: bool,
    authz: Option<&AuthorizationBlock>,
) -> AgentScope {
    let mut granted: Vec<Capability> = registry
        .iter()
        .map(|t| t.required_capability)
        .filter(|cap| allow_logs || *cap != Capability::LogRecent)
        .collect();
    granted.dedup();
    // Respect the central PDP when present: never expose a capability the manager
    // did not grant (the local policy can only narrow further, not widen).
    if let Some(a) = authz {
        granted.retain(|cap| a.scope.granted.contains(cap));
    }
    AgentScope {
        granted,
        mode: ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    }
}

/// Resolve the session subject from the manager authorization, falling back to a
/// stable local identity on single-machine / remote-signaling links.
fn subject_for(authz: Option<&AuthorizationBlock>) -> (String, String, Option<String>) {
    match authz {
        Some(a) => (
            a.actor
                .user_id
                .map(|id| format!("user:{id}"))
                .unwrap_or_else(|| DIRECT_AGENT_ACTOR.to_string()),
            a.device
                .device_id
                .map(|id| format!("device:{id}"))
                .unwrap_or_else(|| "local".to_string()),
            None,
        ),
        None => (DIRECT_AGENT_ACTOR.to_string(), "local".to_string(), None),
    }
}

/// Drive one agentic turn over the given seams, streaming frames to `sink`. Emits
/// a `TurnStarted` frame, runs the loop (whose tool/answer lifecycle the bridge
/// maps to frames), then maps the terminal outcome (or a transport error) to the
/// stream's single terminal frame.
#[allow(clippy::too_many_arguments)]
pub async fn run_direct_agent_turn<S: DiagnoseFrameSink>(
    session: &dyn SessionSeam,
    model: &dyn ModelSeam,
    tools: &dyn ToolSeam,
    registry: &[RegisteredTool],
    request_id: &str,
    request: DiagnoseRequestData,
    scope: AgentScope,
    actor_id: String,
    device_id: String,
    tenant_id: Option<String>,
    sink: S,
) {
    let turn_id = format!("{request_id}-t0");
    let mut bridge = StreamingTurnSink::new(sink, request_id);
    bridge.turn_started(&turn_id);

    let clock = now_rfc3339;
    let now = clock();
    let deps = LoopDeps {
        session_seam: session,
        model,
        tools,
        registry,
        response_format: ResponseFormatSpec::None,
        system_prompt: build_agentic_system_message(request.locale.as_deref()),
        max_context_bytes: desk_diagnose_core::DEFAULT_MAX_CONTEXT_BYTES,
        clock: &clock,
    };
    let claim = ClaimTurnParams {
        conversation_id: request_id.to_string(),
        tenant_id,
        actor_id,
        device_id,
        // No durable policy revision on the Direct runtime; the scope is computed
        // fresh per request from the local policy + authorization.
        policy_revision: 0,
        current_pdp_scope: scope,
        turn_id: turn_id.clone(),
        request_id: Some(request_id.to_string()),
        connection_id: None,
        now,
    };
    let user = ChatMessage::text(format!("{request_id}-u0"), ChatRole::User, request.question);
    match run_agent_turn(&deps, claim, user, &mut bridge).await {
        Ok(outcome) => bridge.finish_outcome(&outcome),
        Err(transport) => bridge.error(transport),
    }
}

// ---------------------------------------------------------------------------
// Test-only seams: a scripted model and a fixed read-tool result, so the handler
// tests and this module's tests drive the loop deterministically (no network).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod test_seams {
    use super::*;
    use desk_agent_protocol::AgentError;
    use desk_diagnose_core::chat::{ModelTurn, ToolCall};
    use desk_diagnose_core::seam::{ModelRequest, ToolRunOutput, TurnSink};
    use std::sync::Mutex;

    /// A model that returns queued turns in order (Send + Sync via a Mutex).
    pub struct ScriptedModel {
        turns: Mutex<std::collections::VecDeque<ModelTurn>>,
    }
    impl ScriptedModel {
        pub fn new(turns: Vec<ModelTurn>) -> Self {
            Self {
                turns: Mutex::new(turns.into()),
            }
        }
    }
    #[async_trait::async_trait(?Send)]
    impl ModelSeam for ScriptedModel {
        async fn call(
            &self,
            _request: ModelRequest,
            _sink: &mut dyn TurnSink,
        ) -> Result<ModelTurn, AgentError> {
            Ok(self
                .turns
                .lock()
                .unwrap()
                .pop_front()
                .expect("a scripted model turn"))
        }
    }

    /// A read-tool seam that returns a fixed JSON result for any read call.
    #[derive(Default)]
    pub struct FakeReadTools;
    #[async_trait::async_trait(?Send)]
    impl ToolSeam for FakeReadTools {
        async fn run_read(&self, call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
            Ok(ToolRunOutput {
                content: format!("{{\"tool\":\"{}\",\"ok\":true}}", call.name),
                image_data_url: None,
            })
        }
    }
}

#[cfg(test)]
impl DirectAgentRuntime {
    /// A runtime whose model returns the given scripted turns and whose read tools
    /// return a fixed result — for handler tests that drive the agentic path
    /// without a network.
    pub fn for_test(
        turns: Vec<desk_diagnose_core::chat::ModelTurn>,
        settings: Arc<SharedSettings>,
    ) -> Self {
        Self::with_seams(
            Arc::new(test_seams::ScriptedModel::new(turns)),
            Arc::new(test_seams::FakeReadTools),
            Arc::new(InMemorySessionSeam::new()),
            settings,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::settings::Settings;
    use desk_agent_protocol::authz::{AuthzActor, AuthzDevice};
    use desk_agent_protocol::diagnose::{DiagnoseEvent, DiagnoseEventKind};
    use desk_agent_protocol::{AgentScope, RiskLevel};
    use desk_diagnose_core::chat::{ModelTurn, StopReason, ToolCall};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn settings(allow_logs: bool) -> Arc<SharedSettings> {
        let mut s = Settings::default();
        s.collection_policy.allow_logs = allow_logs;
        Arc::new(SharedSettings::from(s))
    }

    fn request(q: &str) -> DiagnoseRequestData {
        DiagnoseRequestData {
            question: q.into(),
            include_screen: false,
            context_kinds: vec![],
            locale: None,
        }
    }

    fn authz(granted: Vec<Capability>) -> AuthorizationBlock {
        AuthorizationBlock {
            version: 1,
            scope: AgentScope {
                granted,
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            orchestrator_grants: vec!["ai.diagnose".into()],
            max_risk: RiskLevel::Low,
            actor: AuthzActor { user_id: Some(7) },
            device: AuthzDevice { device_id: Some(9) },
            request_id: "req".into(),
            session_id: None,
            expires_at: None,
            issuer: "manager".into(),
            audience: "device".into(),
            signature: None,
        }
    }

    /// `allow_logs = false` removes `log.recent` from the granted read scope; the
    /// other read capabilities remain, read-only.
    #[test]
    fn scope_gates_logs_on_policy() {
        let reg = read_tool_registry();
        let with_logs = direct_read_scope(&reg, true, None);
        assert!(with_logs.granted.contains(&Capability::LogRecent));
        assert!(with_logs.granted.contains(&Capability::SystemInfo));
        assert_eq!(with_logs.mode, ExecutionMode::ReadOnly);

        let no_logs = direct_read_scope(&reg, false, None);
        assert!(!no_logs.granted.contains(&Capability::LogRecent));
        assert!(no_logs.granted.contains(&Capability::SystemInfo));
    }

    /// A manager authorization narrows the scope to its granted intersection — a
    /// capability the local policy would allow but the PDP withheld is dropped.
    #[test]
    fn scope_intersects_manager_authz() {
        let reg = read_tool_registry();
        let block = authz(vec![Capability::SystemInfo, Capability::ProcessList]);
        let scope = direct_read_scope(&reg, true, Some(&block));
        assert!(scope.granted.contains(&Capability::SystemInfo));
        assert!(scope.granted.contains(&Capability::ProcessList));
        assert!(!scope.granted.contains(&Capability::NetworkPorts));
        assert!(!scope.granted.contains(&Capability::LogRecent));
    }

    /// The subject resolves from the authorization when present, else the local
    /// fallback identity.
    #[test]
    fn subject_resolution() {
        let (actor, device, tenant) = subject_for(None);
        assert_eq!(actor, "local-operator");
        assert_eq!(device, "local");
        assert!(tenant.is_none());

        let block = authz(vec![Capability::SystemInfo]);
        let (actor, device, _) = subject_for(Some(&block));
        assert_eq!(actor, "user:7");
        assert_eq!(device, "device:9");
    }

    /// A recording frame sink collecting every emitted `DiagnoseEvent`.
    fn recorder() -> (Rc<RefCell<Vec<DiagnoseEvent>>>, impl Fn(DiagnoseEvent)) {
        let store = Rc::new(RefCell::new(Vec::new()));
        let s = store.clone();
        (store, move |e| s.borrow_mut().push(e))
    }

    fn tool_use(id: &str, name: &str) -> ModelTurn {
        ModelTurn {
            stop_reason: StopReason::ToolUse,
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: name.into(),
                arguments_json: "{}".into(),
            }],
            ..Default::default()
        }
    }

    fn answer(text: &str) -> ModelTurn {
        ModelTurn {
            text: text.into(),
            stop_reason: StopReason::EndTurn,
            ..Default::default()
        }
    }

    /// A read-tool turn streams TurnStarted → ToolStarted → ToolFinished →
    /// Answer, terminating exactly once.
    #[tokio::test]
    async fn drives_read_tool_then_answer_to_frames() {
        let runtime = DirectAgentRuntime::for_test(
            vec![
                tool_use("c1", "read_system_info"),
                answer("the host is healthy"),
            ],
            settings(true),
        );
        let (store, sink) = recorder();
        runtime
            .run("req-1", request("how is it?"), None, sink)
            .await;

        let ev = store.borrow();
        let kinds: Vec<_> = ev.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                DiagnoseEventKind::TurnStarted,
                DiagnoseEventKind::ToolStarted,
                DiagnoseEventKind::ToolFinished,
                DiagnoseEventKind::Answer,
            ]
        );
        assert_eq!(ev[1].tool_name.as_deref(), Some("read_system_info"));
        assert_eq!(ev[2].tool_ok, Some(true));
        assert_eq!(
            ev.last().unwrap().answer.as_deref(),
            Some("the host is healthy")
        );
        assert_eq!(ev.iter().filter(|e| e.is_terminal()).count(), 1);
        // seq is monotonic and gapless.
        assert_eq!(
            ev.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    /// An immediate answer (no tool call) streams TurnStarted → Answer.
    #[tokio::test]
    async fn drives_immediate_answer_to_frames() {
        let runtime = DirectAgentRuntime::for_test(vec![answer("all good")], settings(true));
        let (store, sink) = recorder();
        runtime.run("req-2", request("status?"), None, sink).await;
        let ev = store.borrow();
        assert_eq!(
            ev.iter().map(|e| e.kind).collect::<Vec<_>>(),
            vec![DiagnoseEventKind::TurnStarted, DiagnoseEventKind::Answer]
        );
    }
}
