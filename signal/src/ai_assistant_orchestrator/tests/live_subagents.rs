//! Opt-in live-model evidence through the production OSS entry and durable stores.
//! The host route is synthetic; these cases never approve or invoke a device tool.
use super::*;
use desk_diagnose_core::{chat::TokenUsage, session::PersistedAgentSession};
use sea_orm::{ColumnTrait, QueryFilter};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

struct LiveWarnings;
impl log::Log for LiveWarnings {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn && metadata.target().contains("assistant_model")
    }
    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!("LIVE_WARNING {}", record.args());
        }
    }
    fn flush(&self) {}
}

fn config() -> crate::model_provider::ModelProviderConfig {
    crate::model_provider::ModelProviderConfig {
        wire_protocol: Some(desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions),
        model: Some(std::env::var("LRD_LIVE_DEEPSEEK_MODEL").unwrap()),
        base_url: Some(std::env::var("LRD_LIVE_DEEPSEEK_BASE_URL").unwrap()),
        api_key: Some(std::env::var("LRD_LIVE_DEEPSEEK_API_KEY").unwrap()),
        request_options: json!({"thinking":{"type":"disabled"}}),
        max_context_bytes: Some(131_072),
        runtime_max_output_tokens: 4096,
        ..Default::default()
    }
}

async fn host(connections: &SharedConnectionMap) {
    use desk_signal_facade::model::{
        auth_context::AuthContext, connection::ConnectionModel, signal::RemoteDeskTypeEnum,
        version::VersionInfo,
    };
    let request = actix_web::test::TestRequest::get()
        .insert_header(("upgrade", "websocket"))
        .insert_header(("connection", "upgrade"))
        .insert_header(("sec-websocket-version", "13"))
        .insert_header(("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="))
        .to_http_request();
    let payload = <web::Payload as actix_web::FromRequest>::from_request(
        &request,
        &mut actix_web::dev::Payload::None,
    )
    .await
    .unwrap();
    let (_, socket, _stream) = actix_ws::handle(&request, payload).unwrap();
    connections.write().await.insert(
        "live-fixture-host".into(),
        ConnectionState {
            model: ConnectionModel {
                connection_id: "live-fixture-host".into(),
                ip: None,
                version_info: VersionInfo::new(
                    1,
                    1,
                    "fixture".into(),
                    RemoteDeskTypeEnum::Server,
                    None,
                    Some("device".into()),
                ),
                device_id: None,
                owner_node_id: None,
            },
            session: std::sync::Arc::new(tokio::sync::RwLock::new(socket)),
            terminal_connection_ids: Default::default(),
            request_callback_map: Default::default(),
            device_code: None,
            auth_context: AuthContext::token_auth(1, 1, RemoteDeskTypeEnum::Server),
        },
    );
}

#[actix_web::test]
async fn formatted_delegation_calls_create_children_without_rewriting_model_history() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let originals = ["first", "second"].map(|name| {
        serde_json::to_string_pretty(&json!({
            "task":format!("Analyze {name} supplied value"), "required_for_completion":false,
            "name":name,"acceptance_criteria":["Report only the supplied value"]
        }))
        .unwrap()
    });
    let delta = json!({"choices":[{"delta":{"role":"assistant","tool_calls": originals.iter().enumerate().map(|(index, args)| json!({
        "index":index,"id":format!("spawn-{index}"),"type":"function",
        "function":{"name":"spawn_subagent","arguments":args}
    })).collect::<Vec<_>>()}}]});
    let reply = format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}],\"usage\":{{\"prompt_tokens\":4,\"completion_tokens\":3}}}}\n\ndata: [DONE]\n\n"
    );
    let capture = actix_web::rt::spawn(async move {
        capture_one_openai_request_with_sse(&listener, &reply).await;
        let ending = json!({"choices":[{"delta":{"content":"children-created"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2}});
        capture_one_openai_request_with_sse(
            &listener,
            &format!("data: {ending}\n\ndata: [DONE]\n\n"),
        )
        .await;
    });
    let db = Database::connect("sqlite::memory:").await.unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::ai_assistant_gate::enable_test_host();
    crate::model_provider::save(
        &db,
        crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("synthetic".into()),
            base_url: Some(format!("http://{address}")),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131072),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    run_turn_inner(
        web::Data::new(SharedConnectionMap::new()),
        db.clone(),
        "format-turn".into(),
        "controller".into(),
        "offline".into(),
        1,
        "device".into(),
        AiAssistantAsk {
            question: "Delegate two optional independent analyses".into(),
            client_message_id: "original-message".into(),
            conversation_id: Some("formatted-delegation".into()),
            ..Default::default()
        },
        None,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(10), capture)
        .await
        .unwrap()
        .unwrap();
    let children = crate::entity::agent_subagent_run::Entity::find()
        .all(&db)
        .await
        .unwrap();
    assert_eq!(children.len(), 2);
    assert!(children.iter().all(|child| child.state == "queued"));
    let root = derive_conversation_key("1", "device", Some("formatted-delegation"), "unused");
    let snapshot = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .read_snapshot(&root)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        latest_committed_answer(&snapshot).as_deref(),
        Some("children-created")
    );
    let calls = snapshot
        .messages
        .iter()
        .flat_map(|message| &message.tool_calls)
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2);
    for (call, original) in calls.iter().zip(originals) {
        assert_eq!(call.arguments_json, original);
    }
    db.close().await.unwrap();
}

fn output_json(answer: &str) -> Option<Value> {
    let answer = answer.trim();
    let answer = answer
        .strip_prefix("```json")
        .or_else(|| answer.strip_prefix("```"))
        .unwrap_or(answer)
        .trim()
        .trim_end_matches("```")
        .trim();
    serde_json::from_str(answer).ok()
}

// This separates factual correctness from the requested answer wrapper. The
// strict acceptance still rejects prose surrounding the final JSON object.
fn embedded_output_json(answer: &str) -> Option<Value> {
    let offset = answer.find('{')?;
    let mut values = serde_json::Deserializer::from_str(&answer[offset..]).into_iter::<Value>();
    let value = values.next()?.ok()?;
    value.is_object().then_some(value)
}

fn correct(case: &str, value: &Value) -> bool {
    match case {
        "SA-001" => value["answer"] == 42,
        "SA-002" => value["findings"].as_array().is_some_and(|rows| {
            let expected = [
                ("storage", "insufficient_disk_space", "ENOSPC"),
                ("dependency", "dependency_timeout", "deadline exceeded"),
                ("listener", "address_in_use", "EADDRINUSE"),
            ];
            rows.len() == expected.len()
                && expected.into_iter().all(|(id, cause, evidence)| {
                    let matches = rows
                        .iter()
                        .filter(|row| row["id"] == id)
                        .collect::<Vec<_>>();
                    matches.len() == 1
                        && matches[0]["cause"] == cause
                        && matches[0]["evidence"]
                            .as_str()
                            .is_some_and(|text| text.contains(evidence))
                })
        }),
        "SA-003" => {
            value["ids"] == json!(["a", "b", "c"])
                && value["sources"]["a"] == json!(["left"])
                && value["sources"]["b"] == json!(["left", "right"])
                && value["sources"]["c"] == json!(["right"])
        }
        "delegation_smoke" => value["answer"] == 42 && value["ids"] == json!(["a", "b", "c"]),
        _ => false,
    }
}

// Missing provider counters remain unknown; known subtotals are separate evidence.
fn aggregate_usage(
    receipts: impl IntoIterator<Item = Option<TokenUsage>>,
) -> ([Option<i64>; 4], [i64; 4], usize) {
    let mut totals = [Some(0_i64); 4];
    let mut known = [0_i64; 4];
    let mut unknown_calls = 0;
    for receipt in receipts {
        let counters = receipt
            .map(|tokens| {
                [
                    tokens.input_tokens,
                    tokens.output_tokens,
                    tokens.cache_read_tokens,
                    tokens.cache_write_tokens,
                ]
            })
            .unwrap_or([None; 4]);
        if counters[0].is_none() || counters[1].is_none() {
            unknown_calls += 1;
        }
        for (index, value) in counters.into_iter().enumerate() {
            if let Some(value) = value {
                known[index] += value;
            }
            totals[index] = totals[index].zip(value).map(|(sum, value)| sum + value);
        }
    }
    (totals, known, unknown_calls)
}

#[actix_web::test]
async fn comparison_findings_reject_extra_duplicate_and_missing_rows() {
    let valid = json!({"findings":[
        {"id":"storage","cause":"insufficient_disk_space","evidence":"ENOSPC"},
        {"id":"dependency","cause":"dependency_timeout","evidence":"deadline exceeded"},
        {"id":"listener","cause":"address_in_use","evidence":"EADDRINUSE"}
    ]});
    assert!(correct("SA-002", &valid));
    let mut extra = valid.clone();
    extra["findings"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"invented","cause":"unsupported","evidence":"none"}));
    assert!(!correct("SA-002", &extra));
    let mut duplicate = valid.clone();
    duplicate["findings"][2] = valid["findings"][0].clone();
    assert!(!correct("SA-002", &duplicate));
    let mut missing = valid.clone();
    missing["findings"].as_array_mut().unwrap().pop();
    assert!(!correct("SA-002", &missing));
    let mut wrong = valid.clone();
    wrong["findings"][0]["cause"] = json!("dependency_timeout");
    assert!(!correct("SA-002", &wrong));
}

fn arm_accepts_children(arm: &str, child_count: usize) -> bool {
    match arm {
        "single_agent_sequential_tools" | "single_agent_parallel_tools" => child_count == 0,
        "main_with_subagents" | "explicit_delegation" => true,
        _ => false,
    }
}

#[actix_web::test]
async fn comparison_evidence_rejects_baseline_delegation_and_preserves_unknown_usage() {
    for arm in [
        "single_agent_sequential_tools",
        "single_agent_parallel_tools",
    ] {
        assert!(arm_accepts_children(arm, 0));
        assert!(!arm_accepts_children(arm, 1));
    }
    assert!(arm_accepts_children("main_with_subagents", 2));
    assert!(!arm_accepts_children("unrecognized", 0));
    let full = || {
        Some(TokenUsage {
            input_tokens: Some(10),
            output_tokens: Some(2),
            cache_read_tokens: Some(0),
            cache_write_tokens: None,
        })
    };
    assert_eq!(
        aggregate_usage([full(), full()]),
        ([Some(20), Some(4), Some(0), None], [20, 4, 0, 0], 0)
    );
    assert_eq!(
        aggregate_usage([full(), None, full()]),
        ([None; 4], [20, 4, 0, 0], 1)
    );
    assert_eq!(
        aggregate_usage([Some(TokenUsage {
            input_tokens: None,
            output_tokens: Some(3),
            cache_read_tokens: None,
            cache_write_tokens: Some(0)
        })]),
        ([None, Some(3), None, Some(0)], [0, 3, 0, 0], 1)
    );
}

fn shuffled_arms(seed: &mut u64) -> [&'static str; 3] {
    let mut arms = [
        "single_agent_sequential_tools",
        "single_agent_parallel_tools",
        "main_with_subagents",
    ];
    for index in (1..arms.len()).rev() {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        arms.swap(index, (*seed % (index as u64 + 1)) as usize);
    }
    arms
}

#[actix_web::test]
async fn comparison_order_is_reproducible_and_covers_each_arm_once() {
    let mut first = 57281;
    let mut replay = first;
    let mut orders = std::collections::BTreeSet::new();
    for _ in 0..15 {
        let arms = shuffled_arms(&mut first);
        assert_eq!(arms, shuffled_arms(&mut replay));
        assert_eq!(
            arms.iter().collect::<std::collections::BTreeSet<_>>().len(),
            3
        );
        orders.insert(arms);
    }
    assert!(orders.len() > 1);
}

#[derive(Clone, Copy)]
struct ComparisonCutoff {
    instant: Instant,
    unix_ms: i64,
}

impl ComparisonCutoff {
    fn after(duration: Duration) -> Self {
        Self {
            instant: Instant::now() + duration,
            unix_ms: chrono::Utc::now().timestamp_millis() + duration.as_millis() as i64,
        }
    }

    async fn run<T>(self, future: impl std::future::Future<Output = T>) -> Result<T, ()> {
        // An already expired arm must not get even one first poll/model request.
        if Instant::now() >= self.instant {
            return Err(());
        }
        tokio::time::timeout_at(tokio::time::Instant::from_std(self.instant), future)
            .await
            .map_err(|_| ())
    }
}

#[actix_web::test]
async fn comparison_cutoff_does_not_restart_for_later_arms_or_poll_expired_work() {
    let cutoff = ComparisonCutoff::after(Duration::from_millis(10));
    let first = cutoff;
    let second = cutoff;
    assert_eq!(first.instant, second.instant);
    assert_eq!(first.unix_ms, second.unix_ms);
    assert!(
        first
            .run(futures_util::future::pending::<()>())
            .await
            .is_err()
    );
    let polled = std::cell::Cell::new(false);
    assert!(
        second
            .run(async {
                polled.set(true);
            })
            .await
            .is_err()
    );
    assert!(!polled.get());
}

async fn sample(
    case: &str,
    arm: &str,
    repetition: usize,
    prompt: String,
    cutoff: ComparisonCutoff,
) -> Value {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::model_provider::save(&db, config()).await.unwrap();
    crate::ai_assistant_gate::enable_test_host();
    let connections = web::Data::new(SharedConnectionMap::new());
    host(connections.as_ref()).await;
    let client = format!("live-{case}-{arm}-{repetition}");
    let root = derive_conversation_key("1", "device", Some(&client), "unused");
    let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db.clone());
    let store = crate::agent_subagent_store::SubAgentStore::new(db.clone());
    let started = Instant::now();
    let started_at_ms = chrono::Utc::now().timestamp_millis();
    let mut initial_wait_evidence = Value::Null;
    let execution = cutoff
        .run(async {
            run_turn_inner(
                connections.clone(),
                db.clone(),
                format!("live-{client}"),
                "controller".into(),
                "live-fixture-host".into(),
                1,
                "device".into(),
                AiAssistantAsk {
                    question: prompt,
                    client_message_id: "original-message".into(),
                    conversation_id: Some(client.clone()),
                    ..Default::default()
                },
                None,
            )
            .await;
            if let Some(row) = crate::entity::agent_session::Entity::find()
                .filter(crate::entity::agent_session::Column::ConversationId.eq(&root))
                .one(&db)
                .await
                .unwrap()
            {
                let state = PersistedAgentSession::decode_json(&row.state_json).unwrap();
                initial_wait_evidence = json!({
                    "durable_wait":state.subagent_wait.is_some(),
                    "lease_released":row.lease_deadline.is_none(),
                    "unfinished_children":store.queued_task_candidates(0,32).await.unwrap().len(),
                    "configured_model_max_inflight":std::env::var("LRD_MODEL_MAX_INFLIGHT").ok(),
                });
            }
            let mut driver_errors = Vec::new();
            for _ in 0..12 {
                let queued = store.queued_task_candidates(0, 32).await.unwrap();
                let mut runtimes = Vec::new();
                for child in queued {
                    if let Some(runtime) =
                        store.child_runtime_candidate(&child.task_id).await.unwrap()
                    {
                        runtimes.push(runtime);
                    }
                }
                let progressed = !runtimes.is_empty();
                let outcomes =
                    futures_util::future::join_all(runtimes.into_iter().map(|runtime| {
                        let connections = connections.clone();
                        let db = db.clone();
                        async move {
                            resume_subagent_turn(
                                connections,
                                db,
                                &crate::ai_assistant_gate::global_ai_assistant_gate(),
                                runtime,
                            )
                            .await
                        }
                    }))
                    .await;
                for outcome in outcomes {
                    if let Err(error) = outcome {
                        driver_errors.push(format!("child {:?}: {}", error.kind, error.message));
                    }
                }
                store
                    .resolve_parent_wait(&root, "1", "device")
                    .await
                    .unwrap();
                if let Some(runtime) = store
                    .parent_runtime_candidate(&root, "1", "device")
                    .await
                    .unwrap()
                {
                    if let Err(error) = resume_subagent_turn(
                        connections.clone(),
                        db.clone(),
                        &crate::ai_assistant_gate::global_ai_assistant_gate(),
                        runtime,
                    )
                    .await
                    {
                        driver_errors.push(format!("parent {:?}: {}", error.kind, error.message));
                    }
                } else if !progressed {
                    break;
                }
            }
            driver_errors
        })
        .await;
    let elapsed_ms = started.elapsed().as_millis();
    let snapshot = sessions.read_snapshot(&root).await.unwrap();
    let children = crate::entity::agent_subagent_run::Entity::find()
        .all(&db)
        .await
        .unwrap();
    let receipts = crate::entity::model_egress_receipt::Entity::find()
        .all(&db)
        .await
        .unwrap();
    let reservations = crate::entity::agent_delegation_reservation::Entity::find()
        .all(&db)
        .await
        .unwrap();
    let group = crate::entity::agent_delegation_group::Entity::find()
        .one(&db)
        .await
        .unwrap();
    let ledger = group
        .as_ref()
        .map(|group| serde_json::from_str::<Value>(&group.state_json).unwrap());
    let (usage, known_usage, unknown_usage) = aggregate_usage(receipts.iter().map(|receipt| {
        receipt
            .usage_json
            .as_ref()
            .map(|encoded| serde_json::from_str::<TokenUsage>(encoded).unwrap())
    }));
    let reservation_evidence = reservations.iter().map(|row| json!({
        "state":row.state,"kind":row.operation_kind,
        "reservation":serde_json::from_str::<Value>(&row.reservation_json).unwrap(),
        "actual":row.actual_json.as_ref().map(|raw| serde_json::from_str::<Value>(raw).unwrap())
    })).collect::<Vec<_>>();
    let Some(snapshot) = snapshot else {
        let result = json!({"case":case,"arm":arm,"repetition":repetition,
            "passed":false,"functional_passed":false,"answer_wrapper_accepted":false,
            "failure":"no_saved_parent_snapshot","timed_out":execution.is_err(),
            "comparison_deadline_unix_ms":cutoff.unix_ms,"started_at_unix_ms":started_at_ms,
            "elapsed_ms":elapsed_ms,"model_calls":receipts.len(),
            "input_tokens":usage[0],"output_tokens":usage[1],"cache_read_tokens":usage[2],"cache_write_tokens":usage[3],
            "unknown_usage_calls":unknown_usage,"group_ledger":ledger,"reservation_evidence":reservation_evidence,
            "per_call_usage":receipts.iter().map(|r| r.usage_json.as_ref().map(|u| serde_json::from_str::<TokenUsage>(u).unwrap())).collect::<Vec<_>>()});
        db.close().await.unwrap();
        return result;
    };
    let answer = latest_committed_answer(&snapshot).unwrap_or_default();
    let parent_receipts = reservations
        .iter()
        .filter(|r| r.conversation_id == root)
        .filter_map(|r| r.provider_receipt_id.as_ref())
        .collect::<std::collections::BTreeSet<_>>();
    let states = children
        .iter()
        .map(|child| child.state.as_str())
        .collect::<Vec<_>>();
    let child_sessions = crate::entity::agent_session::Entity::find()
        .filter(crate::entity::agent_session::Column::ConversationId.ne(&root))
        .all(&db)
        .await
        .unwrap();
    let forbidden_child_calls = child_sessions
        .iter()
        .map(|row| PersistedAgentSession::decode_json(&row.state_json).unwrap())
        .flat_map(|session| session.conversation)
        .flat_map(|message| message.tool_calls)
        .filter(|call| {
            desk_diagnose_core::subagent::tools::effect(&call.name).is_some()
                || call.name.contains("goal")
                || call.name.contains("schedule")
        })
        .count();
    let parent_calls = snapshot
        .messages
        .iter()
        .flat_map(|message| &message.tool_calls)
        .collect::<Vec<_>>();
    let all_tool_names = parent_calls
        .iter()
        .map(|call| call.name.clone())
        .chain(
            child_sessions
                .iter()
                .map(|row| PersistedAgentSession::decode_json(&row.state_json).unwrap())
                .flat_map(|session| session.conversation)
                .flat_map(|message| message.tool_calls)
                .map(|call| call.name),
        )
        .collect::<Vec<_>>();
    let business_tool_proposals = all_tool_names
        .iter()
        .filter(|name| {
            desk_diagnose_core::subagent::tools::effect(name).is_none()
                && name.as_str() != "describe_tools"
                && name.as_str()
                    != desk_diagnose_core::task_status_tools::UPDATE_TASK_STATUS_TOOL_NAME
        })
        .count();
    let device_actions = crate::entity::agent_action_item::Entity::find()
        .all(&db)
        .await
        .unwrap()
        .len();
    let commands = crate::entity::agent_exec_task::Entity::find()
        .all(&db)
        .await
        .unwrap()
        .len();
    let requested_reads = parent_calls
        .iter()
        .filter(|call| call.name == desk_diagnose_core::subagent::tools::RESULT)
        .filter_map(|call| serde_json::from_str::<Value>(&call.arguments_json).ok())
        .filter_map(|input| input["task_id"].as_str().map(str::to_owned))
        .collect::<std::collections::BTreeSet<_>>();
    let parent_row = crate::entity::agent_session::Entity::find()
        .filter(crate::entity::agent_session::Column::ConversationId.eq(&root))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let parent = PersistedAgentSession::decode_json(&parent_row.state_json).unwrap();
    let read_tasks = parent
        .accepted_subagent_observations
        .iter()
        .map(|accepted| accepted.result.task_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let interpreted_tasks = parent
        .interpreted_subagent_results
        .iter()
        .map(|accepted| accepted.result.task_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let functional_passed = execution.as_ref().is_ok_and(|errors| errors.is_empty())
        && snapshot.terminal_error.is_none()
        && embedded_output_json(&answer).is_some_and(|value| correct(case, &value))
        && children.iter().all(|child| child.state == "completed")
        && arm_accepts_children(arm, children.len())
        && forbidden_child_calls == 0
        && business_tool_proposals == 0
        && device_actions == 0
        && commands == 0
        && (case != "SA-001" || children.is_empty())
        && (case != "delegation_smoke"
            || (children.len() == 2
                && children.iter().all(|child| {
                    read_tasks.contains(&child.task_id)
                        && interpreted_tasks.contains(&child.task_id)
                })));
    let answer_wrapper_accepted = output_json(&answer).is_some_and(|value| correct(case, &value));
    let passed = functional_passed && answer_wrapper_accepted;
    let per_call_usage = receipts
        .iter()
        .map(|receipt| {
            let usage = receipt
                .usage_json
                .as_ref()
                .map(|encoded| serde_json::from_str::<TokenUsage>(encoded).unwrap());
            json!({"receipt_id":receipt.receipt_id,"usage":usage})
        })
        .collect::<Vec<_>>();
    let result = json!({"case":case,"arm":arm,"repetition":repetition,"passed":passed,
        "initial_wait_evidence":initial_wait_evidence,
        "per_call_usage":per_call_usage,"reservation_evidence":reservation_evidence,
        "comparison_deadline_unix_ms":cutoff.unix_ms,"started_at_unix_ms":started_at_ms,
        "timed_out":execution.is_err(),
        "functional_passed":functional_passed,"answer_wrapper_accepted":answer_wrapper_accepted,
        "elapsed_ms":elapsed_ms,"answer":answer,"child_count":children.len(),"child_states":states,
        "driver_errors":execution.ok(),"terminal_error_kind":snapshot.terminal_error.as_ref().map(|error| format!("{:?}",error.kind)),
        "terminal_error_message":snapshot.terminal_error.as_ref().map(|error| &error.message),
        "child_failure_reasons":children.iter().map(|child| serde_json::from_str::<Value>(&child.state_json).unwrap()["failure_reason"].clone()).collect::<Vec<_>>(),
        "child_reports":children.iter().map(|child| serde_json::from_str::<Value>(&child.state_json).unwrap()["terminal_report"].clone()).collect::<Vec<_>>(),
        "model_calls":receipts.len(),"input_tokens":usage[0],"output_tokens":usage[1],"cache_read_tokens":usage[2],
        "cache_write_tokens":usage[3],"known_usage_subtotals":{
            "input_tokens":known_usage[0],"output_tokens":known_usage[1],
            "cache_read_tokens":known_usage[2],"cache_write_tokens":known_usage[3]},
        "arm_policy_satisfied":arm_accepts_children(arm, children.len()),
        "unknown_usage_calls":unknown_usage,"parent_peak_context_bytes":receipts.iter().filter(|r| parent_receipts.contains(&r.receipt_id)).map(|r| r.total_bytes).max(),
        "group_ledger":ledger,"call_kinds":reservations.iter().map(|r| (&r.operation_kind,&r.state)).collect::<Vec<_>>(),
        "device_calls":device_actions + commands,"business_tool_proposals":business_tool_proposals,
        "tool_names":all_tool_names,
        "child_report_reads_requested":requested_reads.len(),"child_reports_read":read_tasks.len(),"child_reports_interpreted":interpreted_tasks.len(),"forbidden_child_calls":forbidden_child_calls,
        "child_report_repair_counts":children.iter().map(|child| serde_json::from_str::<Value>(&child.state_json).unwrap()["report_corrections_used"].clone()).collect::<Vec<_>>()});
    println!(
        "LIVE_SUBAGENT_RESULT {}",
        serde_json::to_string(&result).unwrap()
    );
    db.close().await.unwrap();
    result
}

#[actix_web::test]
#[ignore = "requires explicit project-model credentials and public network access"]
async fn live_project_model_subagent_corpus() {
    static WARNINGS: LiveWarnings = LiveWarnings;
    let _ = log::set_logger(&WARNINGS);
    log::set_max_level(log::LevelFilter::Warn);
    let corpus: Value = serde_json::from_str(
        &std::fs::read_to_string(std::env::var("LRD_LIVE_SUBAGENT_CORPUS_PATH").unwrap()).unwrap(),
    )
    .unwrap();
    let selected = std::env::var("LRD_LIVE_SUBAGENT_CASE").unwrap_or_else(|_| "all".into());
    let count = std::env::var("LRD_LIVE_SUBAGENT_REPETITIONS")
        .ok()
        .map(|s| s.parse::<usize>().unwrap())
        .unwrap_or(5);
    assert!((1..=5).contains(&count));
    let seed = std::env::var("LRD_LIVE_SUBAGENT_COMPARISON_SEED")
        .expect("runner must record an explicit randomization seed")
        .parse::<u64>()
        .unwrap();
    assert_ne!(seed, 0);
    let mut random_state = seed;
    let mut results = Vec::new();
    for repetition in 0..count {
        if selected == "all" || selected == "delegation_smoke" {
            results.push(sample("delegation_smoke", "explicit_delegation", repetition,
                "请明确创建两个必需的独立子任务：一个计算 17+25，另一个对 [a,b,b,c] 去重并排序。两个任务都只使用上述输入，不需要设备操作。等待两个任务真正完成，读取报告后汇总。主会话最终只输出 JSON：{\"answer\":42,\"ids\":[\"a\",\"b\",\"c\"]}。请通过子任务工具实际委派，不要自行替代这两个任务。".into(), ComparisonCutoff::after(Duration::from_secs(300))).await);
        }
        for case in ["SA-001", "SA-002", "SA-003"] {
            if selected != "all" && selected != "quality" && selected != case {
                continue;
            }
            let task = match case {
                "SA-001" => {
                    "只计算 17 加 25，给出结果。最终仅输出 JSON 对象，answer 字段为数值。".into()
                }
                "SA-002" => format!(
                    "分别分析以下三份独立日志，给出每份日志的原因与对应证据，并汇总。日志：{}。最终仅输出 JSON 对象 findings 数组，每项含 id、cause、evidence。cause 使用 insufficient_disk_space/dependency_timeout/address_in_use，evidence 引用该源原始日志片段。所有所需数据已给出，无需读取设备或联网。",
                    json!(
                        corpus["fixtures"]["independent_logs"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|log| json!({"id":log["id"],"content":log["content"]}))
                            .collect::<Vec<_>>()
                    )
                ),
                _ => format!(
                    "核对并合并两份独立数据源：left={}，right={}。最终仅输出 JSON 对象，ids 为排序且去重的 ID 数组，sources 为以 ID 为键的 JSON 对象，每个值为该 ID 的来源字符串数组（left 在 right 前）。所有所需数据已给出，无需读取设备或联网。",
                    corpus["fixtures"]["duplicate_rows"]["left"],
                    corpus["fixtures"]["duplicate_rows"]["right"]
                ),
            };
            // All three arms retain this same external cutoff, even when an
            // earlier arm exhausts it. Durable source deadlines are not rewritten.
            let cutoff = ComparisonCutoff::after(Duration::from_secs(900));
            for arm in shuffled_arms(&mut random_state) {
                let policy = match arm {
                    "single_agent_sequential_tools" => {
                        "本轮由你自己完成，不创建子任务；需要工具时按顺序调用。"
                    }
                    "single_agent_parallel_tools" => {
                        "本轮由你自己完成，不创建子任务；确有独立工具需求时可并行调用。"
                    }
                    _ => {
                        "本轮自行判断是否值得创建子任务；简单任务直接完成，需要委派时使用子任务工具。"
                    }
                };
                results
                    .push(sample(case, arm, repetition, format!("{task}\n{policy}"), cutoff).await);
            }
        }
    }
    let artifact = std::env::var("LRD_LIVE_SUBAGENT_EVIDENCE_PATH").unwrap();
    std::fs::write(artifact, serde_json::to_vec_pretty(&json!({"schema_version":1,
        "scope":"production_oss_entry_real_model_synthetic_device_route_no_device_io",
        "answer_acceptance":"passed requires correct output as a complete JSON object or complete fenced JSON object; functional_passed also accepts the first JSON object after explanatory prose, with all runtime checks unchanged",
        "profile":{"model":std::env::var("LRD_LIVE_DEEPSEEK_MODEL").unwrap(),"thinking":"disabled","max_context_bytes":131072,"output_limit":4096},
        "randomization":{"seed":seed,"algorithm":"xorshift64_fisher_yates_per_case_repetition","execution_order":"results array order"},
        "cache_method":"observed per-call usage; no equal-cache cost or latency claim",
        "deadline_method":"one 900-second external absolute cutoff per case/repetition shared by all three arms; immutable production source deadlines remain independently recorded in group ledgers",
        "comparison_limitations":["tool-free inputs make the two single-agent tool strategies degenerate","provider cache residency is observed, not controlled","arms execute serially in randomized order; later arms have less remaining time to the shared external cutoff; start times recorded"],
        "results":results})).unwrap()).unwrap();
    assert!(!results.is_empty());
    assert!(
        results.iter().all(|result| result["passed"] == true),
        "see synthetic result evidence"
    );
}
