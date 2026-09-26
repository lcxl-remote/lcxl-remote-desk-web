//! Source-bound, bounded turn probes return only to their original worker pipe.
use super::*;
use desk_agent_protocol::computer_turn::ComputerActionTurnStatus;
use desk_ipc_protocol::message::{ComputerTurnQueryPayload, ComputerTurnStatusPayload};
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};
use std::time::{Duration, Instant};

#[derive(Default)]
pub(super) struct Routes {
    pending: StdMutex<HashMap<String, Pending>>,
}
struct Pending {
    worker: (Option<WorkerKey>, WorkerIncarnation),
    request: ComputerTurnQueryPayload,
    sender: mpsc::UnboundedSender<ServiceToWorker>,
    expires: Instant,
}
impl Routes {
    fn register(&self, entry: Pending) -> bool {
        let Ok(mut pending) = self.pending.lock() else {
            return false;
        };
        pending.retain(|_, item| item.expires > Instant::now() && !item.sender.is_closed());
        if pending.contains_key(&entry.request.request_id) {
            return false;
        }
        pending.retain(|_, item| item.worker != entry.worker);
        if pending.len() >= 256 {
            return false;
        }
        pending.insert(entry.request.request_id.clone(), entry);
        true
    }

    fn allows(&self, model: &SignalingModel, authority: Option<&str>) -> bool {
        let Ok(pending) = self.pending.lock() else {
            return false;
        };
        pending.get(&model.request_id).is_some_and(|entry| {
            entry.expires > Instant::now()
                && !entry.sender.is_closed()
                && Some(entry.request.authority.as_str()) == authority
                && model.to_connection_id.is_none()
                && model.response_state.is_none()
                && model.get_data().ok().as_ref() == Some(&entry.request.query)
        })
    }

    fn complete(&self, model: &SignalingModel, authority: Option<&str>) {
        if model.from_connection_id.is_some()
            || !model
                .response_state
                .as_ref()
                .is_some_and(|state| state.is_success())
        {
            return;
        }
        let Ok(status) = model.get_data::<ComputerActionTurnStatus>() else {
            return;
        };
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        let Some(entry) = pending.get(&model.request_id) else {
            return;
        };
        if entry.expires <= Instant::now()
            || Some(entry.request.authority.as_str()) != authority
            || entry.request.query != status.query
        {
            return;
        }
        let entry = pending
            .remove(&model.request_id)
            .expect("checked pending probe");
        let _ = entry.sender.send(ServiceToWorker::ComputerTurnStatus(
            ComputerTurnStatusPayload {
                request: entry.request,
                state: status.state,
            },
        ));
    }
}

impl WorkerManager {
    pub(crate) async fn register_turn_query(
        &self,
        key: Option<&WorkerKey>,
        incarnation: WorkerIncarnation,
        request: &ComputerTurnQueryPayload,
    ) -> bool {
        if request.query.validate().is_err()
            || request.control_generation == 0
            || uuid::Uuid::parse_str(&request.request_id).is_err()
            || request.authority.len() != 64
            || !request.authority.bytes().all(|b| b.is_ascii_hexdigit())
            || key.is_some_and(|key| key.desktop != DesktopTarget::LinuxSession)
        {
            return false;
        }
        let inner = self.inner.lock().await;
        let worker = match key {
            Some(key) => inner.resident_workers.get(key),
            None if !self.uses_session_targeting() => inner.active_worker.as_ref(),
            _ => None,
        };
        let Some(worker) = worker.filter(|worker| worker.incarnation == incarnation) else {
            return false;
        };
        self.turn_queries.register(Pending {
            worker: (key.cloned(), incarnation),
            request: request.clone(),
            sender: worker.ipc_tx.clone(),
            expires: Instant::now() + Duration::from_secs(5),
        })
    }

    pub(crate) fn turn_query_allowed_on(&self, text: &str, authority: Option<&str>) -> bool {
        let Ok(model) = serde_json::from_str::<SignalingModel>(text) else {
            return true;
        };
        model.signaling_type != SignalingType::QueryComputerActionTurn
            || self.turn_queries.allows(&model, authority)
    }

    pub(crate) fn complete_turn_query(&self, model: &SignalingModel, authority: Option<&str>) {
        if model.signaling_type == SignalingType::ComputerActionTurnStatus {
            self.turn_queries.complete(model, authority);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{
        computer_turn::{ComputerActionTurnQuery, ComputerActionTurnState},
        computer_use::ComputerActionTurnScope,
    };

    fn request() -> ComputerTurnQueryPayload {
        ComputerTurnQueryPayload {
            request_id: uuid::Uuid::new_v4().to_string(),
            authority: "a".repeat(64),
            control_generation: 1,
            query: ComputerActionTurnQuery {
                actor_id: "7".into(),
                scope: ComputerActionTurnScope {
                    conversation_id: "conversation".into(),
                    turn_id: "turn".into(),
                    input_revision: 1,
                    lease_token: 2,
                },
            },
        }
    }
    fn outgoing(request: &ComputerTurnQueryPayload) -> SignalingModel {
        SignalingModel::new(
            &request.request_id,
            SignalingType::QueryComputerActionTurn,
            None,
            None,
            Some(serde_json::to_value(&request.query).unwrap()),
            None,
        )
    }
    fn reply(request: &ComputerTurnQueryPayload) -> SignalingModel {
        SignalingModel::success_response(
            &request.request_id,
            SignalingType::ComputerActionTurnStatus,
            None,
            None,
            Some(&ComputerActionTurnStatus {
                query: request.query.clone(),
                state: ComputerActionTurnState::Revoked,
            }),
        )
        .unwrap()
    }
    fn entry(
        request: ComputerTurnQueryPayload,
        sender: mpsc::UnboundedSender<ServiceToWorker>,
        incarnation: u64,
    ) -> Pending {
        Pending {
            worker: (None, WorkerIncarnation::for_test(incarnation)),
            request,
            sender,
            expires: Instant::now() + Duration::from_secs(5),
        }
    }
    #[test]
    fn routing_is_source_bound_and_replies_return_only_once_to_the_original_pipe() {
        let routes = Routes::default();
        let request = request();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        assert!(routes.register(entry(request.clone(), sender, 1)));
        let query = outgoing(&request);
        assert!(routes.allows(&query, Some(&request.authority)));
        assert!(!routes.allows(&query, None));
        assert!(!routes.allows(&query, Some(&"b".repeat(64))));
        let mut wrong = request.clone();
        wrong.query.scope.turn_id = "other".into();
        assert!(!routes.allows(&outgoing(&wrong), Some(&request.authority)));
        routes.complete(&reply(&wrong), Some(&request.authority));
        routes.complete(&reply(&request), Some(&"b".repeat(64)));
        let mut peer = reply(&request);
        peer.from_connection_id = Some("browser".into());
        routes.complete(&peer, Some(&request.authority));
        assert!(receiver.try_recv().is_err());
        routes.complete(&reply(&request), Some(&request.authority));
        let ServiceToWorker::ComputerTurnStatus(received) = receiver.try_recv().unwrap() else {
            panic!("status expected")
        };
        assert_eq!(received.request, request);
        assert_eq!(received.state, ComputerActionTurnState::Revoked);
        routes.complete(&reply(&request), Some(&request.authority));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn replacing_a_probe_or_worker_never_redirects_an_old_reply() {
        let routes = Routes::default();
        let first = request();
        let second = request();
        let replacement = request();
        let (old_tx, mut old_rx) = mpsc::unbounded_channel();
        let (new_tx, mut new_rx) = mpsc::unbounded_channel();
        assert!(routes.register(entry(first.clone(), old_tx.clone(), 1)));
        assert!(!routes.register(entry(first.clone(), new_tx.clone(), 2)));
        assert!(routes.register(entry(second.clone(), old_tx, 1)));
        assert!(routes.register(entry(replacement.clone(), new_tx, 2)));
        routes.complete(&reply(&first), Some(&first.authority));
        assert!(old_rx.try_recv().is_err());
        routes.complete(&reply(&second), Some(&second.authority));
        assert!(old_rx.try_recv().is_ok());
        assert!(new_rx.try_recv().is_err());
        routes.complete(&reply(&replacement), Some(&replacement.authority));
        assert!(new_rx.try_recv().is_ok());
    }

    #[test]
    fn expired_queries_are_neither_sent_nor_completed() {
        let routes = Routes::default();
        let request = request();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut pending = entry(request.clone(), sender, 1);
        pending.expires = Instant::now() - Duration::from_secs(1);
        assert!(routes.register(pending));
        assert!(!routes.allows(&outgoing(&request), Some(&request.authority)));
        routes.complete(&reply(&request), Some(&request.authority));
        assert!(receiver.try_recv().is_err());
    }
}
