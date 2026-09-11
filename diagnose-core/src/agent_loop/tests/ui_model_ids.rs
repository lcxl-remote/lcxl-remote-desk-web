use super::*;

struct IdReadTools(RefCell<Vec<ToolCall>>);
#[async_trait(?Send)]
impl ToolSeam for IdReadTools {
    async fn run_read(&self, call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        self.0.borrow_mut().push(call.clone());
        Ok(ToolRunOutput {
            content: "observed UI".into(),
            image_data_url: None,
        })
    }
}

#[tokio::test]
async fn model_ids_resolve_for_execution_without_rewriting_original_proposal() {
    for expired in [false, true] {
        let broad = AgentScope {
            granted: vec![Capability::DesktopUiInspect],
            ..scope()
        };
        let sess = MemSession::default();
        let mut initial = PersistedAgentSession::new(
            "conv",
            "actor",
            "device",
            1,
            broad.clone(),
            "2026-09-11T00:00:00Z",
        );
        initial.conversation.push(ChatMessage::assistant_tool_calls(
            "prior-call",
            "",
            vec![crate::chat::ToolCallRef {
                id: "prior-read".into(),
                name: "inspect_desktop_session".into(),
                arguments_json: "{}".into(),
            }],
        ));
        initial.conversation.push(ChatMessage::tool_result("prior-result", "prior-read", serde_json::json!({"ReadContext":{"DesktopSessionInspect":{
            "session":{"token":"session","snapshot_id":"server-snapshot","object_kind":"desktop_session","expires_at":if expired {"2026-09-10T00:00:00Z"}else{"2030-01-01T00:00:00Z"}},
            "os":"macos","interactive_session_incarnation":"worker"
        }}}).to_string()));
        *sess.inner.borrow_mut() = Some(initial);
        let input = r#"{"root_id":"session","query":{"any":["Calendar"]}}"#;
        let requests = Rc::new(RefCell::new(vec![]));
        let model = ScriptModel {
            turns: RefCell::new(
                [
                    tool_use_args("inspect", "inspect_desktop_ui", input),
                    answer("done"),
                ]
                .into(),
            ),
            requests: requests.clone(),
        };
        let tools = IdReadTools(RefCell::new(vec![]));
        let registry = vec![read_tool(
            "inspect_desktop_ui",
            Capability::DesktopUiInspect,
        )];
        let clock = || "2026-09-11T00:00:00Z".to_string();
        let mut sink = Collector(Rc::new(RefCell::new(String::new())));
        let result = run_agent_turn(
            &deps(&sess, &model, &tools, &registry, &clock),
            ClaimTurnParams {
                current_pdp_scope: broad,
                ..claim()
            },
            ChatMessage::text("user", ChatRole::User, "Find Calendar"),
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(result, LoopOutcome::Answered("done".into()));
        if expired {
            assert!(tools.0.borrow().is_empty());
            assert!(
                requests.borrow()[1]
                    .messages
                    .iter()
                    .any(|m| m.text.contains("has expired"))
            );
        } else {
            let calls = tools.0.borrow();
            let args: serde_json::Value = serde_json::from_str(&calls[0].arguments_json).unwrap();
            assert_eq!(args["root"]["snapshot_id"], "server-snapshot");
            assert_eq!(args["root"]["token"], "session");
        }
        let stored = sess.inner.borrow();
        let original = stored
            .as_ref()
            .unwrap()
            .conversation
            .iter()
            .flat_map(|m| &m.tool_calls)
            .find(|c| c.id == "inspect")
            .unwrap();
        assert_eq!(original.arguments_json, input);
    }
}
