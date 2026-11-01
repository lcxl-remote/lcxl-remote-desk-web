use super::*;

#[test]
fn invalid_forget_arguments_reach_the_model_without_dispatch_on_small_stacks() {
    for stack in [1024 * 1024, 2 * 1024 * 1024] {
        std::thread::Builder::new()
            .stack_size(stack)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(Box::pin(async {
                        for (arguments, expected) in [
                            (r#"{"operation":"forget","memory_id":"memory-one"}"#, "missing field `expected_revision`"),
                            (r#"{"operation":"forget","memory_id":"memory-one","expected_revision":1,"expected_revision":2}"#, "duplicate field `expected_revision`"),
                            (r#"{"operation":"forget","memory_id":"memory-one","expected_revision":"PRIVATE_VALUE"}"#, "invalid type; expected u64"),
                        ] {
                            let store=MemSession {
                                data_access:Some(crate::ai_assistant::data_access::AssistantDataToolAccess {
                                    settings:Default::default(),policy_revision:1,memory_state_generation:1,control_generation:1,
                                }),..Default::default()
                            };
                            let params=claim();
                            let mut session=crate::ai_assistant::test_support::personal_session(&params.actor_id,&params.conversation_id,&params.device_id);
                            session.scope_snapshot=params.current_pdp_scope.clone();
                            *store.inner.borrow_mut()=Some(session);
                            let model=ScriptModel{
                                turns:RefCell::new([tool_use_args("invalid-forget",crate::ai_assistant::data_tools::MANAGE_MEMORIES,arguments),answer("correct the memory request")].into()),
                                requests:Rc::new(RefCell::new(Vec::new())),
                            };
                            let calls=Rc::new(RefCell::new(Vec::new()));
                            let tools=RecordingTools{calls:calls.clone(),reply:"unused".into()};
                            let registry=vec![read_tool("sysinfo",Capability::SystemInfo)];
                            let clock=||"2026-10-31T00:00:01Z".into();
                            let outcome=run_agent_turn(&deps(&store,&model,&tools,&registry,&clock),params,ChatMessage::text("owner-input",ChatRole::User,"forget the saved preference"),&mut NullTurnSink).await.unwrap();
                            assert_eq!(outcome,LoopOutcome::Answered("correct the memory request".into()));
                            assert!(calls.borrow().is_empty());
                            let requests=model.requests.borrow();
                            assert_eq!(requests.len(),2);
                            let returned=requests[1].messages.iter().find(|message|message.tool_call_id.as_deref()==Some("invalid-forget")).unwrap();
                            assert!(returned.text.contains(expected),"{}",returned.text);
                            assert!(!returned.text.contains("PRIVATE_VALUE"));
                            assert_eq!(returned.tool_ok,Some(false));
                            let stored=store.inner.borrow();
                            assert!(stored.as_ref().unwrap().unclosed_tool_call_ids().is_empty());
                        }
                    }));
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
