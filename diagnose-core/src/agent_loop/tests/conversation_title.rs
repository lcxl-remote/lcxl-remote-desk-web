use super::*;
use crate::model_profile::ModelUseCase;

struct NamingModel {
    script: ScriptModel,
    title_error: Option<AgentErrorKind>,
}
#[async_trait(?Send)]
impl ModelSeam for NamingModel {
    fn model_output_token_limit(&self, request: &ModelRequest) -> Result<i64, AgentError> {
        self.script.model_output_token_limit(request)
    }
    async fn context_policy(
        &self,
        requirements: crate::model_capability::ModelRequirements,
    ) -> Result<crate::model_context::PinnedContextPolicy, AgentError> {
        self.script.context_policy(requirements).await
    }
    async fn call(
        &self,
        request: ModelRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelTurn, AgentError> {
        if request.use_case == ModelUseCase::ConversationTitle
            && let Some(kind) = &self.title_error
        {
            self.script.requests.borrow_mut().push(request);
            return Err(AgentError {
                kind: *kind,
                message: "synthetic optional naming failure".into(),
                retryable: false,
                safe_for_model: false,
                error_code: None,
            });
        }
        self.script.call(request, sink).await
    }
}

async fn exercise(title_output: &str, expected_title: Option<&str>, temporary: bool) {
    exercise_with_error(title_output, expected_title, temporary, None).await;
}

async fn exercise_with_error(
    title_output: &str,
    expected_title: Option<&str>,
    temporary: bool,
    title_error: Option<AgentErrorKind>,
) {
    let params = claim();
    let mut initial = crate::ai_assistant::test_support::personal_session(
        &params.actor_id,
        &params.conversation_id,
        &params.device_id,
    );
    initial.input_revision = 1;
    initial.assistant_data.as_mut().unwrap().temporary = temporary;
    initial
        .assistant_data
        .as_mut()
        .unwrap()
        .current_actor_input_seq = 37;
    let store = MemSession {
        title_limit: Some(40),
        data_access: Some(crate::ai_assistant::data_access::AssistantDataToolAccess {
            settings: Default::default(),
            policy_revision: 1,
            memory_state_generation: 1,
            control_generation: 1,
        }),
        inner: RefCell::new(Some(initial)),
        ..Default::default()
    };
    let requests = Rc::new(RefCell::new(Vec::new()));
    let model = ScriptModel {
        turns: RefCell::new([answer("The original answer"), answer(title_output)].into()),
        requests: requests.clone(),
    };
    let model = NamingModel {
        script: model,
        title_error,
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(Vec::new())),
        reply: String::new(),
    };
    let clock = || "2026-10-20T00:00:01Z".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = crate::model_message_labels::model_bound_user_message(
        "original-input".into(),
        "Find my report".into(),
        desk_agent_protocol::data_lineage::DestinationIdentity::Model {
            connection_id: "test".into(),
            connection_revision: 1,
            model_id: "test".into(),
            profile_revision: 1,
        },
    );
    let user = user.unwrap();
    let outcome = run_agent_turn(
        &deps(&store, &model, &tools, &[], &clock),
        params,
        user,
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(outcome, LoopOutcome::Answered("The original answer".into()));
    assert_eq!(store.title.borrow().as_deref(), expected_title);
    let saved = store.inner.borrow();
    let saved = saved.as_ref().unwrap();
    assert_eq!(saved.turn_state, TurnState::Idle);
    assert!(saved.conversation.iter().any(
        |message| message.role == ChatRole::Assistant && message.text == "The original answer"
    ));
    assert!(
        !saved
            .conversation
            .iter()
            .any(|message| message.text == title_output)
    );
    let requests = requests.borrow();
    assert_eq!(requests.len(), 2);
    let request = &requests[1];
    assert_eq!(request.use_case, ModelUseCase::ConversationTitle);
    assert_eq!(request.messages.len(), 2);
    assert_eq!(request.messages[1].message_id, "original-input");
    assert_eq!(request.messages[1].text, "Find my report");
    assert!(request.tools.is_empty());
    assert!(!sink.0.borrow().contains("title"));
}

#[tokio::test]
async fn first_answer_generates_a_title_without_publishing_title_json() {
    exercise(
        r#"{"title":"Find my report"}"#,
        Some("Find my report"),
        false,
    )
    .await;
}

#[tokio::test]
async fn malformed_title_keeps_the_primary_answer_successful() {
    exercise("not JSON", None, false).await;
}

#[tokio::test]
async fn temporary_conversation_uses_the_same_naming_without_memory() {
    exercise(
        r#"{"title":"Find my report"}"#,
        Some("Find my report"),
        true,
    )
    .await;
}

#[test]
fn naming_and_primary_answer_fit_small_stacks() {
    for bytes in [1024 * 1024, 2 * 1024 * 1024] {
        std::thread::Builder::new()
            .stack_size(bytes)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(exercise(
                        r#"{"title":"Find my report"}"#,
                        Some("Find my report"),
                        false,
                    ));
            })
            .unwrap()
            .join()
            .unwrap();
    }
}

#[tokio::test]
async fn naming_cancellation_and_transport_failure_leave_the_committed_answer_successful() {
    for kind in [
        AgentErrorKind::Cancelled,
        AgentErrorKind::TransportError,
        AgentErrorKind::PermissionDenied,
    ] {
        exercise_with_error(r#"{"title":"Find my report"}"#, None, false, Some(kind)).await;
    }
}
