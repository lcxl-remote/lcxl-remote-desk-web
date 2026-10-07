//! Bounded FIFO admission for physical calls in the single-account OSS brain.
use desk_agent_protocol::{AgentError, AgentErrorKind};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

const DEFAULT_INFLIGHT: usize = 2;
const MAX_INFLIGHT: usize = 64;
const MAX_WAIT: Duration = Duration::from_secs(60);
static ADMISSION: OnceLock<Result<ModelAdmission, String>> = OnceLock::new();

/// Only the admission queue can construct this proof. Holding a whole agent
/// turn, an approval decision or a background process never holds this permit.
pub(crate) struct ModelPermit {
    _permit: OwnedSemaphorePermit,
}

struct ModelAdmission {
    inflight: Arc<Semaphore>,
    waiters: Arc<Semaphore>,
}

fn configured_limit(value: Option<&str>) -> Result<usize, String> {
    match value {
        None => Ok(DEFAULT_INFLIGHT),
        Some(value) => value
            .parse::<usize>()
            .ok()
            .filter(|limit| (1..=MAX_INFLIGHT).contains(limit))
            .ok_or_else(|| "LRD_MODEL_MAX_INFLIGHT must be an integer in 1..=64".into()),
    }
}

fn busy(message: &str) -> AgentError {
    AgentError {
        kind: AgentErrorKind::ModelUnavailable,
        message: message.into(),
        retryable: true,
        safe_for_model: true,
        error_code: Some(desk_utils::error::DeskErrorCode::AI_PLATFORM_BUSY.code()),
    }
}

fn cancelled() -> AgentError {
    AgentError {
        kind: AgentErrorKind::Cancelled,
        message: "The model request was cancelled.".into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

impl ModelAdmission {
    fn new(limit: usize) -> Self {
        Self {
            inflight: Arc::new(Semaphore::new(limit)),
            waiters: Arc::new(Semaphore::new(limit * 8)),
        }
    }

    async fn acquire(&self, cancel: &CancellationToken) -> Result<ModelPermit, AgentError> {
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        // Semaphore reserves released capacity for older queued acquirers, so
        // try_acquire cannot jump over their FIFO position.
        if let Ok(permit) = self.inflight.clone().try_acquire_owned() {
            return if cancel.is_cancelled() {
                Err(cancelled())
            } else {
                Ok(ModelPermit { _permit: permit })
            };
        }
        let _waiting = self
            .waiters
            .clone()
            .try_acquire_owned()
            .map_err(|_| busy("The AI model request queue is full. Try again later."))?;
        let permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(cancelled()),
            result = tokio::time::timeout(MAX_WAIT, self.inflight.clone().acquire_owned()) => {
                result.map_err(|_| busy("The AI model request timed out while waiting for capacity."))?
                    .map_err(|_| busy("The AI model admission queue is unavailable."))?
            },
        };
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        Ok(ModelPermit { _permit: permit })
    }
}

pub(crate) async fn acquire(cancel: &CancellationToken) -> Result<ModelPermit, AgentError> {
    let queue = ADMISSION
        .get_or_init(|| {
            let value = match std::env::var("LRD_MODEL_MAX_INFLIGHT") {
                Ok(value) => Some(value),
                Err(std::env::VarError::NotPresent) => None,
                Err(std::env::VarError::NotUnicode(_)) => {
                    return Err("LRD_MODEL_MAX_INFLIGHT is not valid Unicode".into());
                }
            };
            configured_limit(value.as_deref()).map(ModelAdmission::new)
        })
        .as_ref()
        .map_err(|message| AgentError {
            kind: AgentErrorKind::ModelUnavailable,
            message: message.clone(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        })?;
    queue.acquire(cancel).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::poll;
    use std::task::Poll;

    #[test]
    fn invalid_operator_limits_fail_closed() {
        assert_eq!(configured_limit(None).unwrap(), 2);
        assert_eq!(configured_limit(Some("1")).unwrap(), 1);
        assert_eq!(configured_limit(Some("64")).unwrap(), 64);
        for value in ["0", "65", "-1", "two", ""] {
            assert!(configured_limit(Some(value)).is_err());
        }
    }

    #[tokio::test]
    async fn one_slot_is_fifo_and_dropped_waiter_does_not_block_the_next_call() {
        let queue = ModelAdmission::new(1);
        let cancel = CancellationToken::new();
        let held = queue.acquire(&cancel).await.unwrap();
        let mut first = Box::pin(queue.acquire(&cancel));
        let mut second = Box::pin(queue.acquire(&cancel));
        assert!(matches!(poll!(&mut first), Poll::Pending));
        assert!(matches!(poll!(&mut second), Poll::Pending));
        drop(held);
        assert!(matches!(poll!(&mut second), Poll::Pending));
        let first_held = first.await.unwrap();
        assert!(matches!(poll!(&mut second), Poll::Pending));
        drop(first_held);
        let held = second.await.unwrap();
        let mut abandoned = Box::pin(queue.acquire(&cancel));
        let mut next = Box::pin(queue.acquire(&cancel));
        assert!(matches!(poll!(&mut abandoned), Poll::Pending));
        assert!(matches!(poll!(&mut next), Poll::Pending));
        drop(abandoned);
        drop(held);
        let _held = next.await.unwrap();
        assert_eq!(queue.waiters.available_permits(), 8);
    }

    #[tokio::test]
    async fn full_queue_and_cancel_do_not_consume_a_physical_slot() {
        let queue = ModelAdmission::new(1);
        let cancel = CancellationToken::new();
        let held = queue.acquire(&cancel).await.unwrap();
        let mut waiters: Vec<_> = (0..8).map(|_| Box::pin(queue.acquire(&cancel))).collect();
        for waiter in &mut waiters {
            assert!(matches!(poll!(waiter), Poll::Pending));
        }
        assert!(matches!(queue.acquire(&cancel).await, Err(error) if error.retryable));
        cancel.cancel();
        for waiter in waiters {
            assert!(matches!(waiter.await, Err(error) if error.kind == AgentErrorKind::Cancelled));
        }
        drop(held);
        let next_cancel = CancellationToken::new();
        let _next = queue.acquire(&next_cancel).await.unwrap();
        assert_eq!(queue.waiters.available_permits(), 8);
    }

    #[tokio::test]
    async fn physical_model_future_releases_its_slot_on_cancellation_and_drop() {
        use crate::{model_dial::SignalModelSeam, model_provider::ModelProviderConfig};
        use desk_diagnose_core::{
            chat::{ChatMessage, ChatRole},
            model_profile::WireProtocol,
            prompt::ResponseFormatSpec,
            seam::{ModelRequest, NullTurnSink},
        };
        let config = ModelProviderConfig {
            base_url: Some("https://model.example/v1".into()),
            model: Some("fixture-model".into()),
            api_key: Some("fixture-only".into()),
            wire_protocol: Some(WireProtocol::OpenAiChatCompletions),
            max_context_bytes: Some(131_072),
            ..Default::default()
        };
        for poll_cancelled in [false, true] {
            let queue = ModelAdmission::new(1);
            let cancel = CancellationToken::new();
            let permit = queue.acquire(&cancel).await.unwrap();
            let seam = SignalModelSeam::from_config(&config)
                .unwrap()
                .with_cancellation(cancel.clone());
            let mut sink = NullTurnSink;
            let request = ModelRequest::text_only(
                vec![ChatMessage::text(
                    "fixture-input",
                    ChatRole::User,
                    "fixture",
                )],
                ResponseFormatSpec::None,
            );
            let call = Box::pin(seam.call_admitted(request, &mut sink, permit));
            let next_cancel = CancellationToken::new();
            let mut next = Box::pin(queue.acquire(&next_cancel));
            assert!(matches!(poll!(&mut next), Poll::Pending));
            if poll_cancelled {
                // Cancellation wins before the fixture endpoint can be contacted.
                cancel.cancel();
                assert!(
                    matches!(call.await, Err(error) if error.kind == AgentErrorKind::Cancelled)
                );
            } else {
                // A discarded physical call future owns and drops the permit.
                drop(call);
            }
            let next_permit = tokio::time::timeout(Duration::from_secs(1), next)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(queue.inflight.available_permits(), 0);
            drop(next_permit);
            assert_eq!(queue.inflight.available_permits(), 1);
            assert_eq!(queue.waiters.available_permits(), 8);
        }
    }
    #[actix_web::test]
    async fn successful_physical_response_releases_single_slot_for_next_call() {
        use crate::{model_dial::SignalModelSeam, model_provider::ModelProviderConfig};
        use desk_diagnose_core::{
            chat::{ChatMessage, ChatRole},
            model_profile::WireProtocol,
            prompt::ResponseFormatSpec,
            seam::{ModelRequest, NullTurnSink},
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = actix_web::rt::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                        let length: usize = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .unwrap()
                            .trim()
                            .parse()
                            .unwrap();
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let frame = serde_json::json!({"choices":[{"delta":{"content":"fixture-success"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1}});
                let body = format!("data: {frame}\n\ndata: [DONE]\n\n");
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
        });
        let config = ModelProviderConfig {
            base_url: Some(format!("http://{address}")),
            model: Some("fixture".into()),
            api_key: Some("fixture-only".into()),
            wire_protocol: Some(WireProtocol::OpenAiChatCompletions),
            max_context_bytes: Some(131_072),
            ..Default::default()
        };
        let seam = SignalModelSeam::from_config(&config).unwrap();
        let queue = ModelAdmission::new(1);
        let cancel = CancellationToken::new();
        let first = queue.acquire(&cancel).await.unwrap();
        let mut next = Box::pin(queue.acquire(&cancel));
        assert!(matches!(poll!(&mut next), Poll::Pending));
        let mut sink = NullTurnSink;
        let request = || {
            ModelRequest::text_only(
                vec![ChatMessage::text("input", ChatRole::User, "fixture")],
                ResponseFormatSpec::None,
            )
        };
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            seam.call_admitted(request(), &mut sink, first),
        )
        .await
        .unwrap();
        assert!(result.is_ok(), "{result:?}");
        let next_permit = tokio::time::timeout(Duration::from_secs(1), next)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(queue.inflight.available_permits(), 0);
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            seam.call_admitted(request(), &mut sink, next_permit),
        )
        .await
        .unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(queue.inflight.available_permits(), 1);
        assert_eq!(queue.waiters.available_permits(), 8);
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();
    }
}
