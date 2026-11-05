//! Bound the body before the SDK decodes JSON or server-sent events.

use std::{collections::HashMap, io, sync::Arc, time::Duration};

use futures_util::{StreamExt, TryStreamExt, stream::BoxStream};
use reqwest_mcp::{
    Client, RequestBuilder, Response, StatusCode,
    header::{ACCEPT, CONTENT_TYPE, HeaderName, HeaderValue},
};
use rmcp::{
    model::ClientJsonRpcMessage,
    transport::streamable_http_client::{
        SseError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
    },
};
use rustls::pki_types::{CertificateDer, pem::PemObject};
use sse_stream::{Sse, SseStream};
use url::Url;

use super::McpClientError;

const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_CA_BYTES: usize = 128 * 1024;
const MCP_SESSION_ID: &str = "mcp-session-id";
const JSON: &str = "application/json";
const SSE: &str = "text/event-stream";

#[derive(Clone)]
pub(super) struct BoundedHttp(Client);

impl BoundedHttp {
    pub(super) async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: Url,
        token: &str,
    ) -> Result<T, McpClientError> {
        let response = self
            .0
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| McpClientError::Unavailable)?;
        if !response.status().is_success() || content_type(&response) != Some(JSON) {
            return Err(McpClientError::Unavailable);
        }
        read_json(response)
            .await
            .map_err(|_| McpClientError::Unavailable)
    }
}

pub(super) fn client(extra_ca_pem: Option<&[u8]>) -> Result<BoundedHttp, McpClientError> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    if let Some(pem) = extra_ca_pem {
        if pem.is_empty() || pem.len() > MAX_CA_BYTES {
            return Err(McpClientError::InvalidConfiguration);
        }
        let before = roots.len();
        for cert in CertificateDer::pem_slice_iter(pem) {
            roots
                .add(cert.map_err(|_| McpClientError::InvalidConfiguration)?)
                .map_err(|_| McpClientError::InvalidConfiguration)?;
        }
        if roots.len() == before {
            return Err(McpClientError::InvalidConfiguration);
        }
    }
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| McpClientError::InvalidConfiguration)?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Client::builder()
        .user_agent("lcxl-remote-desk-issue-client")
        .tls_backend_preconfigured(tls)
        .http1_only()
        .redirect(reqwest_mcp::redirect::Policy::none())
        .retry(reqwest_mcp::retry::never())
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .timeout(super::CALL_TIMEOUT)
        .build()
        .map(BoundedHttp)
        .map_err(|_| McpClientError::InvalidConfiguration)
}

type HttpError = StreamableHttpError<reqwest_mcp::Error>;
type EventStream = BoxStream<'static, Result<Sse, SseError>>;

impl StreamableHttpClient for BoundedHttp {
    type Error = reqwest_mcp::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, HttpError> {
        let response = request(
            self.0.post(uri.as_ref()),
            session_id.as_deref(),
            auth,
            headers,
        )
        .json(&message)
        .send()
        .await?;
        match response.status() {
            StatusCode::ACCEPTED | StatusCode::NO_CONTENT => {
                return Ok(StreamableHttpPostResponse::Accepted);
            }
            StatusCode::NOT_FOUND if session_id.is_some() => return Err(HttpError::SessionExpired),
            status if !status.is_success() => {
                return Err(HttpError::UnexpectedServerResponse(
                    "MCP HTTP request failed".into(),
                ));
            }
            _ => {}
        }
        let session = response
            .headers()
            .get(MCP_SESSION_ID)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        match content_type(&response) {
            Some(SSE) => Ok(StreamableHttpPostResponse::Sse(
                event_stream(response),
                session,
            )),
            Some(JSON) => Ok(StreamableHttpPostResponse::Json(
                read_json(response).await?,
                session,
            )),
            _ => Err(HttpError::UnexpectedContentType(None)),
        }
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<EventStream, HttpError> {
        let mut request = request(self.0.get(uri.as_ref()), Some(&session_id), auth, headers);
        if let Some(id) = last_event_id {
            request = request.header("last-event-id", id);
        }
        let response = request.send().await?;
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            return Err(HttpError::ServerDoesNotSupportSse);
        }
        if response.status().is_success() && content_type(&response) == Some(SSE) {
            return Ok(event_stream(response));
        }
        Err(HttpError::UnexpectedServerResponse(
            "MCP event stream unavailable".into(),
        ))
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), HttpError> {
        // The SDK does not read a body for session cleanup.
        self.0.delete_session(uri, session_id, auth, headers).await
    }
}

fn request(
    builder: RequestBuilder,
    session_id: Option<&str>,
    auth: Option<String>,
    headers: HashMap<HeaderName, HeaderValue>,
) -> RequestBuilder {
    let mut builder = builder.header(ACCEPT, "application/json, text/event-stream");
    if let Some(session_id) = session_id {
        builder = builder.header(MCP_SESSION_ID, session_id);
    }
    if let Some(token) = auth {
        builder = builder.bearer_auth(token);
    }
    // Only the SDK supplies protocol headers; connection configuration cannot add them.
    builder.headers(headers.into_iter().collect())
}

fn content_type(response: &Response) -> Option<&str> {
    response
        .headers()
        .get(CONTENT_TYPE)?
        .to_str()
        .ok()?
        .split(';')
        .next()
}

fn bounded_bytes(response: Response) -> impl futures_util::Stream<Item = io::Result<bytes::Bytes>> {
    let mut total = 0_usize;
    response.bytes_stream().map(move |chunk| {
        let chunk = chunk.map_err(io::Error::other)?;
        total = total.saturating_add(chunk.len());
        if total > MAX_BODY_BYTES {
            return Err(io::Error::other("MCP response exceeds its byte limit"));
        }
        Ok(chunk)
    })
}

async fn read_json<T: serde::de::DeserializeOwned>(response: Response) -> Result<T, HttpError> {
    let body = bounded_bytes(response)
        .try_fold(Vec::new(), |mut body, bytes| async move {
            body.extend_from_slice(&bytes);
            Ok(body)
        })
        .await?;
    Ok(serde_json::from_slice(&body)?)
}

fn event_stream(response: Response) -> EventStream {
    SseStream::from_bytes_stream(bounded_bytes(response)).boxed()
}
