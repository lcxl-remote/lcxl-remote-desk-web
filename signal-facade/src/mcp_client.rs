//! Streamable HTTP client for administrator-configured assistant connections.

mod http;
pub mod issue_connection;
pub mod replies;
pub mod settings;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

use std::time::Duration;

use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, JsonObject, PaginatedRequestParams, Tool},
    service::RunningService,
    transport::streamable_http_client::{
        StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
    },
};
use serde_json::Value;
use url::Url;

const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PAGES: usize = 8;
const MAX_TOOLS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpClientError {
    InvalidConfiguration,
    Unavailable,
    InvalidCatalog,
    TargetChanged,
    ToolFailed,
}

impl std::fmt::Display for McpClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidConfiguration => "Invalid MCP connection configuration",
            Self::Unavailable => "MCP connection or response unavailable",
            Self::InvalidCatalog => "MCP tool directory is incomplete or invalid",
            Self::TargetChanged => "External account or issue no longer matches the saved target",
            Self::ToolFailed => "MCP server reported a tool error",
        })
    }
}

impl std::error::Error for McpClientError {}

/// Resolved by the host from trusted configuration. Loading OS roots uses blocking I/O.
#[derive(Clone)]
struct McpEndpoint {
    url: Url,
    bearer_token: String,
    client: http::BoundedHttp,
}

impl McpEndpoint {
    fn new(
        endpoint: &str,
        bearer_token: &str,
        extra_ca_pem: Option<&[u8]>,
    ) -> Result<Self, McpClientError> {
        let url = endpoint_url(endpoint)?;
        if bearer_token.is_empty()
            || bearer_token.len() > 8192
            || !bearer_token.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(McpClientError::InvalidConfiguration);
        }
        Ok(Self {
            url,
            bearer_token: bearer_token.to_owned(),
            client: http::client(extra_ca_pem)?,
        })
    }

    async fn open(&self) -> Result<McpClient, McpClientError> {
        let config = StreamableHttpClientTransportConfig::with_uri(self.url.as_str())
            .auth_header(&self.bearer_token)
            .reinit_on_expired_session(false);
        let transport = StreamableHttpClientTransport::with_client(self.client.clone(), config);
        tokio::time::timeout(CALL_TIMEOUT, ().serve(transport))
            .await
            .map_err(|_| McpClientError::Unavailable)?
            .map(McpClient)
            .map_err(|_| McpClientError::Unavailable)
    }
}

struct McpClient(RunningService<RoleClient, ()>);

impl McpClient {
    async fn list_tools(&self) -> Result<Vec<Tool>, McpClientError> {
        let deadline = tokio::time::Instant::now() + CALL_TIMEOUT;
        let mut cursor = None;
        let mut tools = Vec::new();
        let mut names = std::collections::HashSet::new();
        let mut cursors = std::collections::HashSet::new();
        for _ in 0..MAX_PAGES {
            let page = tokio::time::timeout_at(
                deadline,
                self.0.list_tools(
                    cursor
                        .take()
                        .map(|cursor| PaginatedRequestParams::default().with_cursor(Some(cursor))),
                ),
            )
            .await
            .map_err(|_| McpClientError::Unavailable)?
            .map_err(|_| McpClientError::Unavailable)?;
            if tools.len().saturating_add(page.tools.len()) > MAX_TOOLS {
                return Err(McpClientError::InvalidCatalog);
            }
            for tool in page.tools {
                if tool.name.is_empty() || tool.name.len() > 128 || !names.insert(tool.name.clone())
                {
                    return Err(McpClientError::InvalidCatalog);
                }
                tools.push(tool);
            }
            let Some(next) = page.next_cursor else {
                return Ok(tools);
            };
            if next.is_empty() || next.len() > 4096 || !cursors.insert(next.clone()) {
                return Err(McpClientError::InvalidCatalog);
            }
            cursor = Some(next);
        }
        Err(McpClientError::InvalidCatalog)
    }

    /// The caller binds the exact tool, arguments and external target before dispatch.
    async fn call_tool(
        &self,
        name: String,
        arguments: JsonObject,
    ) -> Result<Value, McpClientError> {
        let result = tokio::time::timeout(
            CALL_TIMEOUT,
            self.0
                .call_tool(CallToolRequestParams::new(name).with_arguments(arguments)),
        )
        .await
        .map_err(|_| McpClientError::Unavailable)?
        .map_err(|_| McpClientError::Unavailable)?;
        if result.is_error == Some(true) {
            return Err(McpClientError::ToolFailed);
        }
        serde_json::to_value(result).map_err(|_| McpClientError::Unavailable)
    }
}

fn endpoint_url(endpoint: &str) -> Result<Url, McpClientError> {
    let url = Url::parse(endpoint).map_err(|_| McpClientError::InvalidConfiguration)?;
    if url.scheme() != "https"
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(McpClientError::InvalidConfiguration);
    }
    Ok(url)
}

#[cfg(test)]
mod tests;
