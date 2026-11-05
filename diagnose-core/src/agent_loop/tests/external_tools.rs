use super::*;
use crate::external_tools::{IssueAccess, IssueTarget, TOOL_NAME};
use crate::seam::{ReadOutcome, ToolOutputFormat};

struct IssueTools {
    selected: Vec<IssueAccess>,
    reads: RefCell<Vec<crate::external_tools::IssueReadMethod>>,
}

#[async_trait(?Send)]
impl ToolSeam for IssueTools {
    async fn external_issue_access(
        &self,
        _: &PersistedAgentSession,
    ) -> Result<Vec<IssueAccess>, AgentError> {
        Ok(self.selected.clone())
    }

    async fn run_external_read(
        &self,
        _: &PersistedAgentSession,
        call: &ToolCall,
    ) -> Result<ReadOutcome, AgentError> {
        let (_, method) = crate::external_tools::resolve(&self.selected, call)?;
        self.reads.borrow_mut().push(method);
        Ok(ReadOutcome::Completed {
            output: ToolRunOutput {
                format: ToolOutputFormat::Text,
                content: "Issue reproduction steps".into(),
                image_data_url: None,
                document_preview: None,
            },
            ok: true,
            event_id: None,
            data_envelope: None,
            background_task: None,
        })
    }

    async fn run_read(&self, _: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        panic!("External reads must not enter the device read transport")
    }
}

#[tokio::test]
async fn external_issue_read_uses_host_port_without_device_grants_and_rejects_write_arguments() {
    for method in ["get", "add_comment"] {
        let sess = MemSession::default();
        let selected = IssueAccess {
            connection_ref: "forge".into(),
            target: IssueTarget {
                platform: desk_agent_protocol::external_tools::IssuePlatform::Gitea,
                instance: "https://forge.test/".into(),
                account_id: 3,
                repository_id: 42,
                owner: "org".into(),
                repository: "project".into(),
                issue_number: 4,
                issue_id: 82,
                private: true,
                internal: false,
            },
        };
        let mut turn = tool_use("read-issue", TOOL_NAME);
        turn.tool_calls[0].arguments_json =
            serde_json::json!({"tool_ref":selected.tool_ref(), "args":{"method":method}})
                .to_string();
        let model = ScriptModel {
            turns: RefCell::new([turn, answer("done")].into()),
            requests: Rc::new(RefCell::new(vec![])),
        };
        let tools = IssueTools {
            selected: vec![selected],
            reads: RefCell::new(vec![]),
        };
        let clock = || "2026-11-04T00:00:00Z".to_string();
        let mut sink = Collector(Rc::new(RefCell::new(String::new())));
        let mut input = claim();
        input.current_pdp_scope.granted.clear();
        let result = run_agent_turn(
            &deps(&sess, &model, &tools, &[], &clock),
            input,
            ChatMessage::text("u", ChatRole::User, "Read the selected issue"),
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(result, LoopOutcome::Answered("done".into()));
        assert_eq!(tools.reads.borrow().len(), usize::from(method == "get"));
        let requests = model.requests.borrow();
        assert!(requests[0].tools.iter().any(|tool| tool.name == TOOL_NAME));
        assert!(
            requests[1]
                .messages
                .iter()
                .any(|message| message.role == ChatRole::Tool
                    && if method == "get" {
                        message.text.contains("Issue reproduction steps")
                    } else {
                        message.text.contains("read method")
                    })
        );
    }
}
