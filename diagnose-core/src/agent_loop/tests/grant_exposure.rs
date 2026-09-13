use super::*;

struct GrantTools {
    snapshot: RefCell<crate::grant_disclosure::GrantDisclosureSnapshot>,
    reads: std::cell::Cell<usize>,
}

#[async_trait(?Send)]
impl ToolSeam for GrantTools {
    async fn current_grant_disclosure(
        &self,
    ) -> Result<Option<crate::grant_disclosure::GrantDisclosureSnapshot>, AgentError> {
        Ok(Some(self.snapshot.borrow().clone()))
    }
    async fn run_read(&self, _: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        self.reads.set(self.reads.get() + 1);
        self.snapshot.borrow_mut().grants[0].remaining_uses = 0;
        Ok(ToolRunOutput {
            content: "immutable observation".into(),
            image_data_url: None,
        })
    }
}

#[tokio::test]
async fn approval_exposes_without_load_and_last_use_refreshes_next_request() {
    let (mut initial, providers, snapshot) = crate::grant_disclosure::tests::fixture();
    initial.latest_input_seq = 1;
    initial.focus_epoch.input_revision = 1;
    let scope = initial.scope_snapshot.clone();
    let target = providers
        .capability_for_tool("inspect_desktop_session")
        .unwrap()
        .registered_tool();
    let inventory = vec![crate::capability_availability::CapabilityAvailability {
        provider_id: snapshot.grants[0].provider_id.clone(),
        capability_id: snapshot.grants[0].capability_id.clone(),
        tool_name: target.name().into(),
        compiled: true,
        enabled: true,
        connected: true,
        ready: true,
        reason: None,
    }];
    let session = MemSession {
        inner: RefCell::new(Some(initial)),
        ..Default::default()
    };
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new([tool_use_args("read", target.name(), "{}"), answer("done")].into()),
        requests: requests.clone(),
    };
    let tools = GrantTools {
        snapshot: RefCell::new(snapshot),
        reads: std::cell::Cell::new(0),
    };
    let mut registry = vec![target];
    registry.extend(crate::capability_disclosure::capability_discovery_tool_registry());
    registry.extend(crate::permission_tools::permission_planning_tool_registry());
    let clock = || "1970-01-01T00:00:00.500Z".to_string();
    let mut deps = deps(&session, &model, &tools, &registry, &clock);
    deps.provider_registry = Some(&providers);
    deps.capability_inventory = Some(&inventory);
    run_agent_turn(
        &deps,
        ClaimTurnParams {
            current_pdp_scope: scope,
            ..claim()
        },
        ChatMessage::text("u", ChatRole::User, "inspect the session"),
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    assert_eq!(tools.reads.get(), 1);
    let requests = requests.borrow();
    assert!(
        requests[0]
            .tools
            .iter()
            .any(|t| t.name == "inspect_desktop_session")
    );
    assert!(
        !requests[1]
            .tools
            .iter()
            .any(|t| t.name == "inspect_desktop_session")
    );
    assert!(
        requests[1]
            .tools
            .iter()
            .any(|t| t.name == "request_capability_grants")
    );
    assert!(requests[1].messages[0].text.contains("exhausted"));
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|m| m.text.contains("immutable observation"))
    );
}
