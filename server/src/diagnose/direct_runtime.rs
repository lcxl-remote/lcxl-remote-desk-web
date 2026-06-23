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
//! authorization, intersected with the PDP-granted scope. When the runtime is
//! built with [`new_with_exec`](DirectAgentRuntime::new_with_exec) the mutating
//! `exec_command` tool is also exposed — but only under a confirm-or-higher
//! execution mode and always behind operator approval (an `ExecPreview` pushed to
//! the control connection, resolved by `ResolveExec`).
//!
//! Continuation is keyed by a subject-namespaced conversation key derived from
//! the request's (non-authoritative) `conversation_id`: follow-up questions that
//! reuse the same client id continue the same in-memory session, so the model
//! sees the accumulated history. An absent / malformed id falls back to the
//! per-request id, keying a fresh single-question conversation.

use std::sync::Arc;

use desk_agent_protocol::authz::AuthorizationBlock;
use desk_agent_protocol::diagnose::DiagnoseRequestData;
use desk_agent_protocol::{AgentScope, Capability, ExecutionMode};
use desk_diagnose_core::agent_loop::{LoopDeps, run_agent_turn};
use desk_diagnose_core::agentic_prompt::build_agentic_system_message;
use desk_diagnose_core::chat::{ChatMessage, ChatRole};
use desk_diagnose_core::conversation_key::derive_conversation_key;
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
    /// Whether the mutating exec tool is wired (the tool seam has exec parts and
    /// the registry includes `exec_command`). When set, the run scope also grants
    /// `shell.exec.confirmed` under the configured execution mode, so the model can
    /// reach the exec path (always through operator approval).
    exec_enabled: bool,
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
            exec_enabled: false,
        }
    }

    /// Build the production runtime with the mutating exec path enabled: read tools
    /// plus `exec_command`, the exec tool seam wired to classify → operator approval
    /// (an `ExecPreview` pushed to the control connection, resolved by `ResolveExec`)
    /// → local worker execution. The exec tool is only exposed to the model when the
    /// configured execution mode permits mutation (and a manager authorization, when
    /// present, grants `shell.exec.confirmed`).
    pub fn new_with_exec(
        agent: Arc<LocalDeviceAgent>,
        settings: Arc<SharedSettings>,
        support: super::direct_exec::DirectExecSupport,
    ) -> Self {
        let tools = Arc::new(
            DirectToolSeam::new(agent, Arc::new(RegexRedactor::new()), DIRECT_AGENT_ACTOR)
                .with_exec(support.into_parts()),
        );
        let model = Arc::new(AdapterModelSeam::new(
            Arc::new(ProviderAdapterSelector),
            settings.clone(),
        ));
        Self {
            tools,
            model,
            session: Arc::new(InMemorySessionSeam::new()),
            registry: Arc::new(super::agent::agent_tool_registry()),
            settings,
            exec_enabled: true,
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
            exec_enabled: false,
        }
    }

    /// Like [`with_seams`](Self::with_seams) but exposing the exec tool + scope (for
    /// tests that drive the mutating path with injected seams).
    #[cfg(test)]
    pub fn with_exec_seams(
        model: Arc<dyn ModelSeam + Send + Sync>,
        tools: Arc<dyn ToolSeam + Send + Sync>,
        session: Arc<dyn SessionSeam + Send + Sync>,
        settings: Arc<SharedSettings>,
    ) -> Self {
        Self {
            tools,
            model,
            session,
            registry: Arc::new(super::agent::agent_tool_registry()),
            settings,
            exec_enabled: true,
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
        connection_id: Option<String>,
        sink: S,
    ) {
        let (allow_logs, execution_mode) = {
            let s = self.settings.read().await;
            (s.collection_policy.allow_logs, s.ai_model.execution_mode)
        };
        let mut scope = direct_read_scope(&self.registry, allow_logs, authz);
        // When exec is wired, grant the mutating capability + adopt the configured
        // execution mode, so the loop exposes `exec_command` (always through operator
        // approval). A manager authorization, when present, must also grant it.
        if self.exec_enabled
            && mode_allows_exec(execution_mode)
            && authz
                .map(|a| a.scope.granted.contains(&Capability::ShellExecConfirmed))
                .unwrap_or(true)
        {
            scope.granted.push(Capability::ShellExecConfirmed);
            scope.mode = execution_mode;
        }
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
            connection_id,
            sink,
        )
        .await;
    }
}

/// Whether the execution mode permits the mutating exec tool to be exposed (it is
/// always gated behind operator approval afterward). Suggest-only / read-only never
/// expose it; automated is not implemented on this runtime, so it is excluded.
fn mode_allows_exec(mode: ExecutionMode) -> bool {
    matches!(
        mode,
        ExecutionMode::ConfirmEachAction | ExecutionMode::SessionApproved
    )
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
    connection_id: Option<String>,
    sink: S,
) {
    let turn_id = format!("{request_id}-t0");
    let mut bridge = StreamingTurnSink::new(sink, request_id);
    bridge.turn_started(&turn_id);

    // Derive the subject-namespaced storage key from the client's continuation
    // intent. A reused (valid) id continues the same session; absent/malformed
    // falls back to the per-request id (a fresh single-question conversation).
    let conversation_key = derive_conversation_key(
        tenant_id.as_deref(),
        &actor_id,
        &device_id,
        request.conversation_id.as_deref(),
        request_id,
    );

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
        max_steps_per_turn: desk_diagnose_core::MAX_STEPS_PER_TURN,
        clock: &clock,
        // No background lease renewer on the Direct runtime: a conversation has a
        // single in-process owner, so a lapsed lease is never concurrently taken
        // over mid-run (only a later claim of the same id recovers an orphan, and
        // the frontend mints a new id on reset/handoff). The owner's own saves keep
        // the lease fresh while it makes progress.
        heartbeat: None,
    };
    let claim = ClaimTurnParams {
        conversation_id: conversation_key,
        tenant_id,
        actor_id,
        device_id,
        // No durable policy revision on the Direct runtime; the scope is computed
        // fresh per request from the local policy + authorization.
        policy_revision: 0,
        current_pdp_scope: scope,
        turn_id: turn_id.clone(),
        request_id: Some(request_id.to_string()),
        connection_id,
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
            conversation_id: None,
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
            .run("req-1", request("how is it?"), None, None, sink)
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

    /// A fake tool seam that exposes the exec path: it records the `ExecContext`
    /// connection id the loop passed and returns an `Executed` outcome.
    struct FakeExecTools {
        seen_connection: std::sync::Mutex<Option<String>>,
    }
    impl FakeExecTools {
        fn new() -> Self {
            Self {
                seen_connection: std::sync::Mutex::new(None),
            }
        }
    }
    #[async_trait::async_trait(?Send)]
    impl desk_diagnose_core::seam::ToolSeam for FakeExecTools {
        async fn run_read(
            &self,
            _call: &desk_diagnose_core::chat::ToolCall,
        ) -> Result<desk_diagnose_core::seam::ToolRunOutput, desk_agent_protocol::AgentError>
        {
            Ok(desk_diagnose_core::seam::ToolRunOutput {
                content: "{}".into(),
                image_data_url: None,
            })
        }
        async fn confirm_and_exec(
            &self,
            _call: &desk_diagnose_core::chat::ToolCall,
            ctx: &desk_diagnose_core::seam::ExecContext,
        ) -> Result<desk_diagnose_core::seam::ExecOutcome, desk_agent_protocol::AgentError>
        {
            *self.seen_connection.lock().unwrap() = ctx.connection_id.clone();
            Ok(desk_diagnose_core::seam::ExecOutcome::Executed(
                desk_diagnose_core::seam::ToolRunOutput {
                    content: "{\"exit_code\":0}".into(),
                    image_data_url: None,
                },
            ))
        }
    }

    fn settings_confirm_exec() -> Arc<SharedSettings> {
        let mut s = Settings::default();
        s.ai_model.execution_mode = ExecutionMode::ConfirmEachAction;
        Arc::new(SharedSettings::from(s))
    }

    fn exec_tool_use(id: &str) -> ModelTurn {
        ModelTurn {
            stop_reason: StopReason::ToolUse,
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: "exec_command".into(),
                arguments_json: r#"{"command":"Restart-Service X"}"#.into(),
            }],
            ..Default::default()
        }
    }

    /// With exec wired and a confirm-each-action mode, the model's `exec_command`
    /// call streams ToolStarted (awaiting approval) → ToolFinished → Answer, and the
    /// control connection id flows into the seam's `ExecContext`.
    #[tokio::test]
    async fn drives_exec_tool_through_approval_to_answer() {
        let tools = Arc::new(FakeExecTools::new());
        let runtime = DirectAgentRuntime::with_exec_seams(
            Arc::new(test_seams::ScriptedModel::new(vec![
                exec_tool_use("c1"),
                answer("done"),
            ])),
            tools.clone(),
            Arc::new(InMemorySessionSeam::new()),
            settings_confirm_exec(),
        );
        let (store, sink) = recorder();
        runtime
            .run(
                "req-x",
                request("restart the service"),
                None,
                Some("browser-7".into()),
                sink,
            )
            .await;

        let ev = store.borrow();
        assert_eq!(
            ev.iter().map(|e| e.kind).collect::<Vec<_>>(),
            vec![
                DiagnoseEventKind::TurnStarted,
                DiagnoseEventKind::ToolStarted,
                DiagnoseEventKind::ToolFinished,
                DiagnoseEventKind::Answer,
            ]
        );
        // The mutating tool's ToolStarted flags it as awaiting operator approval.
        assert!(ev[1].awaiting_approval);
        assert_eq!(ev[1].tool_name.as_deref(), Some("exec_command"));
        assert_eq!(ev[2].tool_ok, Some(true));
        // The control connection id reached the seam's ExecContext.
        assert_eq!(
            tools.seen_connection.lock().unwrap().as_deref(),
            Some("browser-7")
        );
    }

    /// Without exec wiring (read-only runtime) the exec tool is not exposed, so the
    /// scope grants no mutating capability.
    #[tokio::test]
    async fn read_only_runtime_does_not_expose_exec() {
        let runtime = DirectAgentRuntime::for_test(vec![answer("ok")], settings_confirm_exec());
        // The read-only registry never includes exec_command.
        assert!(!runtime.registry.iter().any(|t| t.name() == "exec_command"));
    }

    /// A model seam that records the `messages` of every request it receives and
    /// returns the next queued answer turn. Lets a test assert that a follow-up
    /// turn's model request carries the prior turn's history.
    struct RecordingModel {
        seen: std::sync::Mutex<Vec<Vec<ChatMessage>>>,
        answers: std::sync::Mutex<std::collections::VecDeque<ModelTurn>>,
    }
    impl RecordingModel {
        fn new(answers: Vec<ModelTurn>) -> Self {
            Self {
                seen: std::sync::Mutex::new(Vec::new()),
                answers: std::sync::Mutex::new(answers.into()),
            }
        }
    }
    #[async_trait::async_trait(?Send)]
    impl ModelSeam for RecordingModel {
        async fn call(
            &self,
            request: desk_diagnose_core::seam::ModelRequest,
            _sink: &mut dyn desk_diagnose_core::seam::TurnSink,
        ) -> Result<ModelTurn, desk_agent_protocol::AgentError> {
            self.seen.lock().unwrap().push(request.messages.clone());
            Ok(self.answers.lock().unwrap().pop_front().expect("an answer"))
        }
    }

    /// Two questions carrying the same `conversation_id` continue one session: the
    /// second turn's model request includes the first turn's user question and
    /// assistant answer. A differing request id per turn must not start a new
    /// conversation when the (subject-namespaced) conversation key matches.
    #[tokio::test]
    async fn same_conversation_id_threads_history_to_model() {
        let model = Arc::new(RecordingModel::new(vec![answer("a1"), answer("a2")]));
        let runtime = DirectAgentRuntime::with_seams(
            model.clone(),
            Arc::new(test_seams::FakeReadTools),
            Arc::new(InMemorySessionSeam::new()),
            settings(true),
        );

        let req1 = DiagnoseRequestData {
            question: "why is cpu high?".into(),
            include_screen: false,
            context_kinds: vec![],
            locale: None,
            conversation_id: Some("cv-1".into()),
        };
        let req2 = DiagnoseRequestData {
            question: "and memory?".into(),
            ..req1.clone()
        };
        let (_s1, sink1) = recorder();
        runtime.run("req-a", req1, None, None, sink1).await;
        let (_s2, sink2) = recorder();
        runtime.run("req-b", req2, None, None, sink2).await;

        let seen = model.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "model called once per turn");
        // Turn 2's request must carry turn 1's user question and assistant answer.
        let turn2: Vec<&str> = seen[1].iter().map(|m| m.text.as_str()).collect();
        assert!(
            turn2.iter().any(|t| t.contains("why is cpu high?")),
            "turn 2 missing prior question: {turn2:?}"
        );
        assert!(
            turn2.iter().any(|t| t.contains("a1")),
            "turn 2 missing prior answer: {turn2:?}"
        );
        assert!(
            turn2.iter().any(|t| t.contains("and memory?")),
            "turn 2 missing current question: {turn2:?}"
        );
    }

    /// Two questions WITHOUT a conversation id (each falling back to its own
    /// request id) start independent conversations: the second turn's model
    /// request must not contain the first turn's content.
    #[tokio::test]
    async fn absent_conversation_id_does_not_thread_history() {
        let model = Arc::new(RecordingModel::new(vec![answer("a1"), answer("a2")]));
        let runtime = DirectAgentRuntime::with_seams(
            model.clone(),
            Arc::new(test_seams::FakeReadTools),
            Arc::new(InMemorySessionSeam::new()),
            settings(true),
        );
        let (_s1, sink1) = recorder();
        runtime
            .run("req-a", request("first question"), None, None, sink1)
            .await;
        let (_s2, sink2) = recorder();
        runtime
            .run("req-b", request("second question"), None, None, sink2)
            .await;

        let seen = model.seen.lock().unwrap();
        let turn2: Vec<&str> = seen[1].iter().map(|m| m.text.as_str()).collect();
        assert!(
            !turn2.iter().any(|t| t.contains("first question")),
            "independent conversations must not share history: {turn2:?}"
        );
    }

    /// An immediate answer (no tool call) streams TurnStarted → Answer.
    #[tokio::test]
    async fn drives_immediate_answer_to_frames() {
        let runtime = DirectAgentRuntime::for_test(vec![answer("all good")], settings(true));
        let (store, sink) = recorder();
        runtime
            .run("req-2", request("status?"), None, None, sink)
            .await;
        let ev = store.borrow();
        assert_eq!(
            ev.iter().map(|e| e.kind).collect::<Vec<_>>(),
            vec![DiagnoseEventKind::TurnStarted, DiagnoseEventKind::Answer]
        );
    }
}
