use std::sync::Mutex;
use desk_diagnose_core::model_observability::{CallSnapshot, ObservationPayload, ObservationPhase,
    ObservabilitySeam, OutputOutcome, RequestOutcome, record_structured_output};

#[derive(Default)]
struct TransportRecorder(Mutex<Vec<ObservationEvent>>);
impl ObservabilitySeam for TransportRecorder {
    fn submit(&self, event: ObservationEvent) { self.0.lock().unwrap().push(event); }
}

fn transport_request(parts: &AdapterParts, recorder: Arc<TransportRecorder>) -> (ModelRequest, ObservationContext) {
    let context = ObservationContext::new("actual-transport".into(), now(), parts.attribution.clone(), recorder);
    let mut request = ModelRequest::text_only(
        vec![ChatMessage::text("user", ChatRole::User, "private-transport-request")], ResponseFormatSpec::None,
    );
    request.observation = Some(context.clone());
    (request, context)
}

fn terminal(recorder: &TransportRecorder, expected: RequestOutcome, http: Option<u16>, content: bool) -> CallSnapshot {
    let events = recorder.0.lock().unwrap();
    let calls: Vec<_> = events.iter().filter(|event| event.phase == ObservationPhase::Terminal
        && matches!(event.payload, ObservationPayload::Call(_))).collect();
    let attempts: Vec<_> = events.iter().filter(|event| event.phase == ObservationPhase::Terminal
        && matches!(event.payload, ObservationPayload::Attempt(_))).collect();
    assert_eq!((calls.len(), attempts.len()), (1, 1), "one original physical request and one terminal outcome");
    let ObservationPayload::Call(call) = &calls[0].payload else { unreachable!(); };
    assert_eq!(call.outcome, expected, "{:?} response with HTTP {http:?}", calls[0].attribution.protocol);
    assert_eq!(call.http_status, http);
    assert_eq!(call.timing.first_content_ms.is_some(), content);
    assert!(call.timing.duration_ms.is_some());
    assert!(events.iter().all(ObservationEvent::is_bounded));
    let encoded = serde_json::to_string(&*events).unwrap();
    for private in ["private-transport-request", "private-provider-error", "private-reasoning", "fixture-secret", "127.0.0.1", "unresolvable-model.invalid"] {
        assert!(!encoded.contains(private));
    }
    call.clone()
}

#[derive(Clone, Copy)]
enum ResponseFraming { Complete, ShortBody, MalformedChunked }

async fn single_response(status: u16, body: String, framing: ResponseFraming) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 4096];
                let read = socket.read(&mut buffer).await.unwrap();
                assert!(read > 0 && request.len() < 1_048_576);
                request.extend_from_slice(&buffer[..read]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let length: usize = headers.lines().find_map(|line| line.strip_prefix("content-length:"))
                        .unwrap().trim().parse().unwrap();
                    if request.len() >= end + 4 + length { break; }
                }
            }
            let (headers, wire_body) = match framing {
                ResponseFraming::Complete | ResponseFraming::ShortBody => (
                    format!("Content-Length: {}", body.len() + if matches!(framing, ResponseFraming::ShortBody) { 80 } else { 0 }),
                    body.into_bytes(),
                ),
                ResponseFraming::MalformedChunked => (
                    "Transfer-Encoding: chunked".to_string(),
                    format!("{:x}\r\n{body}\r\ninvalid-chunk-size\r\n", body.len()).into_bytes(),
                ),
            };
            socket.write_all(format!("HTTP/1.1 {status} fixture\r\nContent-Type: text/event-stream\r\n{headers}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            let middle = wire_body.len() / 2;
            socket.write_all(&wire_body[..middle]).await.unwrap();
            tokio::time::sleep(Duration::from_millis(5)).await;
            socket.write_all(&wire_body[middle..]).await.unwrap();
            socket.shutdown().await.unwrap();
        }).await.unwrap();
    });
    (format!("http://{address}/v1"), task)
}

#[actix_web::test]
async fn both_real_http_dialects_keep_request_output_and_attempt_failure_axes_separate() {
    for protocol in [WireProtocol::OpenAiChatCompletions, WireProtocol::AnthropicMessages] {
        let provider_error = match protocol {
            WireProtocol::OpenAiChatCompletions => "data: {\"error\":{\"message\":\"private-provider-error\"}}\n\n".to_string(),
            WireProtocol::AnthropicMessages => "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"private-provider-error\"}}\n\n".to_string(),
            WireProtocol::OpenAiResponses => unreachable!(),
        };
        let good = sse(protocol, 2);
        let reasoned = match protocol {
            WireProtocol::OpenAiChatCompletions => format!("data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"reasoning_content\":\"private-reasoning\"}},\"finish_reason\":null}}]}}\n\n{good}"),
            WireProtocol::AnthropicMessages => {
                let mut response = good.replace("\"index\":0", "\"index\":1");
                let before = response.find("event: content_block_start").unwrap();
                response.insert_str(before, "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"private-reasoning\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n");
                response
            }
            WireProtocol::OpenAiResponses => unreachable!(),
        };
        let empty = good.replace("corrected", "");
        let truncated = match protocol {
            WireProtocol::OpenAiChatCompletions => good.replace("\"finish_reason\":\"stop\"", "\"finish_reason\":\"length\""),
            WireProtocol::AnthropicMessages => good.replace("\"stop_reason\":\"end_turn\"", "\"stop_reason\":\"max_tokens\""),
            WireProtocol::OpenAiResponses => unreachable!(),
        };
        for (status, body, extra, expected, content, output) in [
            (200, good, ResponseFraming::Complete, RequestOutcome::Returned, true, Some(OutputOutcome::Accepted)),
            (200, reasoned, ResponseFraming::Complete, RequestOutcome::Returned, true, Some(OutputOutcome::Accepted)),
            (200, empty, ResponseFraming::Complete, RequestOutcome::Returned, false, Some(OutputOutcome::EmptyResponse)),
            (200, truncated, ResponseFraming::Complete, RequestOutcome::Returned, true, Some(OutputOutcome::OutputTruncated)),
            (401, "private-provider-error".into(), ResponseFraming::Complete, RequestOutcome::HttpError, false, None),
            (503, "private-provider-error".into(), ResponseFraming::Complete, RequestOutcome::HttpError, false, None),
            (200, provider_error, ResponseFraming::Complete, RequestOutcome::ProviderError, false, None),
            // The adapters accept clean EOF as a response. Its unresolved output
            // remains distinct from a transport decoder's framing error.
            (200, ": heartbeat\n\n".into(), ResponseFraming::ShortBody, RequestOutcome::Returned, false, Some(OutputOutcome::InvalidProtocol)),
            (200, ": heartbeat\n\n".into(), ResponseFraming::MalformedChunked, RequestOutcome::StreamError, false, None),
        ] {
            let (url, server) = single_response(status, body, extra).await;
            let parts = adapter(&url, protocol);
            let recorder = Arc::new(TransportRecorder::default());
            let (request, context) = transport_request(&parts, recorder.clone());
            let result = tokio::time::timeout(Duration::from_secs(5), parts.model.call(request, &mut Output::default())).await.unwrap();
            server.await.unwrap();
            if let Some(expected_output) = output {
                let turn = result.as_ref().unwrap();
                record_structured_output(turn, turn.text == "corrected", Some(&context), now());
                let events = recorder.0.lock().unwrap();
                let observed = events.iter().find_map(|event| match &event.payload {
                    ObservationPayload::Call(call) if event.phase == ObservationPhase::Output => Some(call.output), _ => None,
                });
                assert_eq!(observed, Some(expected_output));
            }
            let call = terminal(&recorder, expected, Some(status), content);
            if expected != RequestOutcome::Returned { assert_eq!(call.output, OutputOutcome::NotEvaluated); }
        }
    }
}

#[actix_web::test]
async fn dns_and_local_tls_failures_are_transport_outcomes_without_headers_or_output() {
    for protocol in [WireProtocol::OpenAiChatCompletions, WireProtocol::AnthropicMessages] {
        // Reserved invalid endpoint: no external model request or paid service.
        let parts = adapter("http://unresolvable-model.invalid/v1", protocol);
        let recorder = Arc::new(TransportRecorder::default());
        let (request, _) = transport_request(&parts, recorder.clone());
        assert!(tokio::time::timeout(Duration::from_secs(20), parts.model.call(request, &mut Output::default()))
            .await.expect("DNS failure is bounded by the test").is_err());
        terminal(&recorder, RequestOutcome::TransportError, None, false);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket.write_all(b"this is not a TLS handshake").await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let parts = adapter(&format!("https://{address}/v1"), protocol);
        let recorder = Arc::new(TransportRecorder::default());
        let (request, _) = transport_request(&parts, recorder.clone());
        assert!(tokio::time::timeout(Duration::from_secs(5), parts.model.call(request, &mut Output::default())).await.unwrap().is_err());
        server.await.unwrap();
        terminal(&recorder, RequestOutcome::TransportError, None, false);
    }
}

#[actix_web::test]
async fn the_actual_provider_header_timeout_and_a_dropped_model_future_have_distinct_terminal_outcomes() {
    for protocol in [WireProtocol::OpenAiChatCompletions, WireProtocol::AnthropicMessages] {
        for dropped in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let accepted = Arc::new(tokio::sync::Notify::new());
            let observed = accepted.clone();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = [0; 4096];
                assert!(socket.read(&mut buffer).await.unwrap() > 0);
                observed.notify_one();
                std::future::pending::<()>().await;
            });
            let parts = adapter(&format!("http://{address}/v1"), protocol);
            let recorder = Arc::new(TransportRecorder::default());
            let (request, _) = transport_request(&parts, recorder.clone());
            let mut output = Output::default();
            {
                let call = parts.model.call(request, &mut output);
                tokio::pin!(call);
                tokio::time::timeout(Duration::from_secs(5), async {
                    tokio::select! {
                        _ = accepted.notified() => {},
                        result = &mut call => panic!("request ended before timeout: {result:?}"),
                    }
                }).await.unwrap();
                if !dropped {
                    tokio::time::pause();
                    tokio::time::advance(Duration::from_secs(181)).await;
                    let result = call.await;
                    tokio::time::resume();
                    assert!(result.is_err());
                }
            }
            server.abort();
            assert!(server.await.unwrap_err().is_cancelled());
            terminal(&recorder, if dropped { RequestOutcome::ObservationIncomplete } else { RequestOutcome::Timeout }, None, false);
            assert!(output.0.is_empty());
        }
    }
}
