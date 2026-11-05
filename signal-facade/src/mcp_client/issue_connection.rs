//! Typed issue operations on the repository verified through REST and MCP.

mod github;

use rmcp::model::{JsonObject, Tool};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use super::{McpClient, McpClientError, McpEndpoint, endpoint_url};

const ISSUE_READ: &str = "issue_read";
const ISSUE_WRITE: &str = "issue_write";
const GET_ME: &str = "get_me";
pub const COMMENT_PAGE_SIZE: usize = 30;

pub use desk_diagnose_core::external_tools::{IssuePlatform, IssueReadOutput, IssueTarget};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub id: u64,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub html_url: String,
    #[serde(default)]
    pub is_pull: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub id: u64,
    pub body: String,
    pub user: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RestComment {
    pub id: u64,
    pub body: String,
    pub user: RestAuthor,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RestAuthor {
    pub id: u64,
    pub login: String,
    #[serde(rename = "type")]
    pub account_type: Option<String>,
}

#[derive(Deserialize)]
struct Account {
    id: u64,
    login: String,
}

#[derive(Deserialize)]
struct Repository {
    id: u64,
    full_name: String,
    private: bool,
    #[serde(default)]
    internal: bool,
    visibility: Option<String>,
}

#[derive(Deserialize)]
struct RestIssue {
    id: u64,
    number: u64,
    html_url: String,
    pull_request: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct IssueConnectionProbe {
    pub account_id: u64,
    pub account_login: String,
    pub can_read_issues: bool,
    pub can_reply: bool,
}

pub use desk_diagnose_core::issue_reply::ReplyReconciliationPage;

/// Owns the validated MCP session and exact outbound bytes across the native
/// dispatch transaction. Sending cannot change either the body or target.
pub struct PreparedReply {
    issue: RemoteIssue,
    body: String,
}

impl PreparedReply {
    pub async fn send(
        self,
    ) -> Result<desk_diagnose_core::issue_reply::IssueReplyReceipt, McpClientError> {
        let comment = self.issue.post_comment(&self.body).await?;
        Ok(desk_diagnose_core::issue_reply::IssueReplyReceipt {
            comment_id: comment.id,
            account_id: self.issue.target.account_id,
            body: comment.body,
        })
    }
}

#[derive(Clone)]
pub struct IssueConnection {
    platform: IssuePlatform,
    instance: Url,
    mcp: McpEndpoint,
}

impl IssueConnection {
    pub async fn reconcile_reply_page(
        &self,
        key: &desk_diagnose_core::issue_reply::IssueReplyReceiptKey,
        page: std::num::NonZeroU32,
    ) -> Result<ReplyReconciliationPage, McpClientError> {
        let comments = self.comment_page(&key.access.target, page).await?;
        let receipt = comments
            .iter()
            .find(|comment| key.matches_receipt(comment.user.id, &comment.body))
            .map(
                |comment| desk_diagnose_core::issue_reply::IssueReplyReceipt {
                    comment_id: comment.id,
                    account_id: comment.user.id,
                    body: comment.body.clone(),
                },
            );
        let next_page = if receipt.is_none() && comments.len() == COMMENT_PAGE_SIZE {
            Some(
                page.get()
                    .checked_add(1)
                    .and_then(std::num::NonZeroU32::new)
                    .ok_or(McpClientError::Unavailable)?,
            )
        } else {
            None
        };
        Ok(ReplyReconciliationPage { receipt, next_page })
    }

    pub async fn read_issue(
        &self,
        target: &IssueTarget,
        method: desk_diagnose_core::external_tools::IssueReadMethod,
    ) -> Result<IssueReadOutput, McpClientError> {
        use desk_diagnose_core::external_tools::IssueReadMethod;
        let issue = self.reopen_issue(target).await?;
        let value = match method {
            IssueReadMethod::Get => json!({"issue":issue.issue()}),
            IssueReadMethod::GetComments => json!({"comments":issue.comments().await?}),
        };
        Ok(IssueReadOutput {
            content: value.to_string(),
            tool_signature: issue.read_signature().into(),
        })
    }

    pub async fn test_connection(&self) -> Result<IssueConnectionProbe, McpClientError> {
        let account: Account = self.get(&["user"]).await?;
        let client = self.mcp.open().await?;
        let tools = client.list_tools().await?;
        verify_account(&client, &tools, &account).await?;
        Ok(IssueConnectionProbe {
            account_id: account.id,
            account_login: account.login,
            can_read_issues: compatible_tool(
                &tools,
                ISSUE_READ,
                ISSUE_ARGS,
                &["get", "get_comments"],
            )
            .is_some(),
            can_reply: self.comment_tool(&tools).is_some(),
        })
    }

    /// The HTTPS origins and credential are resolved from host configuration.
    pub fn new(
        platform: IssuePlatform,
        instance: &str,
        mcp_endpoint: &str,
        token: &str,
        extra_ca_pem: Option<&[u8]>,
    ) -> Result<Self, McpClientError> {
        Ok(Self {
            platform,
            instance: endpoint_url(instance)?,
            mcp: McpEndpoint::new(mcp_endpoint, token, extra_ca_pem)?,
        })
    }

    pub async fn bind_issue(
        &self,
        owner: &str,
        repository: &str,
        number: u64,
    ) -> Result<IssueTarget, McpClientError> {
        Ok(self
            .open_issue(owner, repository, number, None)
            .await?
            .target)
    }

    pub async fn reopen_issue(
        &self,
        expected: &IssueTarget,
    ) -> Result<RemoteIssue, McpClientError> {
        self.open_issue(
            &expected.owner,
            &expected.repository,
            expected.issue_number,
            Some(expected),
        )
        .await
    }

    /// REST pages retain the original comment text and immutable author ID.
    /// Intake and delivery reconciliation remain available when MCP is offline.
    pub async fn comment_page(
        &self,
        expected: &IssueTarget,
        page: std::num::NonZeroU32,
    ) -> Result<Vec<RestComment>, McpClientError> {
        self.resolve_issue(
            &expected.owner,
            &expected.repository,
            expected.issue_number,
            Some(expected),
        )
        .await?;
        let mut url = self.rest_url(&[
            "repos",
            &expected.owner,
            &expected.repository,
            "issues",
            &expected.issue_number.to_string(),
            "comments",
        ])?;
        url.query_pairs_mut()
            .append_pair("page", &page.to_string())
            .append_pair(
                match self.platform {
                    IssuePlatform::Gitea => "limit",
                    IssuePlatform::Github => "per_page",
                },
                &COMMENT_PAGE_SIZE.to_string(),
            );
        let comments: Vec<RestComment> = self
            .mcp
            .client
            .get_json(url, &self.mcp.bearer_token)
            .await?;
        if comments.len() > COMMENT_PAGE_SIZE || comments.iter().any(|comment| comment.id == 0) {
            return Err(McpClientError::Unavailable);
        }
        Ok(comments)
    }

    async fn get<T: DeserializeOwned>(&self, segments: &[&str]) -> Result<T, McpClientError> {
        self.mcp
            .client
            .get_json(self.rest_url(segments)?, &self.mcp.bearer_token)
            .await
    }

    fn rest_url(&self, segments: &[&str]) -> Result<Url, McpClientError> {
        let mut url = self.instance.clone();
        let prefix: &[&str] = match self.platform {
            IssuePlatform::Gitea => &["api", "v1"],
            IssuePlatform::Github if self.instance.as_str() == "https://github.com/" => {
                url = Url::parse("https://api.github.com/")
                    .map_err(|_| McpClientError::InvalidConfiguration)?;
                &[]
            }
            IssuePlatform::Github => &["api", "v3"],
        };
        url.path_segments_mut()
            .map_err(|_| McpClientError::InvalidConfiguration)?
            .pop_if_empty()
            .extend(prefix.iter().copied())
            .extend(segments.iter().copied());
        Ok(url)
    }

    async fn resolve_issue(
        &self,
        owner: &str,
        repository: &str,
        number: u64,
        expected: Option<&IssueTarget>,
    ) -> Result<(IssueTarget, Account, RestIssue), McpClientError> {
        if expected.is_some_and(|saved| {
            saved.platform != self.platform || saved.instance != self.instance.as_str()
        }) {
            return Err(McpClientError::TargetChanged);
        }
        if !valid_name(owner) || !valid_name(repository) || number == 0 || number > i64::MAX as u64
        {
            return Err(McpClientError::InvalidConfiguration);
        }
        let number_segment = number.to_string();
        let account_path = ["user"];
        let repository_path = ["repos", owner, repository];
        let issue_path = ["repos", owner, repository, "issues", &number_segment];
        let (account, repo): (Account, Repository) =
            tokio::try_join!(self.get(&account_path), self.get(&repository_path),)?;
        let (canonical_owner, canonical_repo) = repo
            .full_name
            .split_once('/')
            .filter(|(owner, repo)| valid_name(owner) && valid_name(repo))
            .ok_or(McpClientError::TargetChanged)?;
        if account.id == 0
            || !valid_account(&account.login)
            || repo.id == 0
            || !canonical_owner.eq_ignore_ascii_case(owner)
            || !canonical_repo.eq_ignore_ascii_case(repository)
        {
            return Err(McpClientError::TargetChanged);
        }
        if repo
            .visibility
            .as_deref()
            .is_some_and(|value| !matches!(value, "public" | "private" | "internal"))
        {
            return Err(McpClientError::TargetChanged);
        }
        let mut target = IssueTarget {
            platform: self.platform,
            instance: self.instance.to_string(),
            account_id: account.id,
            repository_id: repo.id,
            owner: canonical_owner.into(),
            repository: canonical_repo.into(),
            issue_number: number,
            issue_id: expected.map_or(0, |saved| saved.issue_id),
            private: repo.private,
            internal: repo.internal || repo.visibility.as_deref() == Some("internal"),
        };
        // Verify the names still resolve to the saved repository before reading its issue.
        if expected.is_some_and(|saved| *saved != target) {
            return Err(McpClientError::TargetChanged);
        }
        let original: RestIssue = self.get(&issue_path).await?;
        if original.id == 0
            || original.number != number
            || original.pull_request.is_some()
            || expected.is_some_and(|saved| saved.issue_id != original.id)
        {
            return Err(McpClientError::TargetChanged);
        }
        endpoint_url(&original.html_url).map_err(|_| McpClientError::TargetChanged)?;
        target.issue_id = original.id;
        Ok((target, account, original))
    }

    async fn open_issue(
        &self,
        owner: &str,
        repository: &str,
        number: u64,
        expected: Option<&IssueTarget>,
    ) -> Result<RemoteIssue, McpClientError> {
        let (target, account, original) = self
            .resolve_issue(owner, repository, number, expected)
            .await?;
        let client = self.mcp.open().await?;
        let tools = client.list_tools().await?;
        let read = compatible_tool(&tools, ISSUE_READ, ISSUE_ARGS, &["get", "get_comments"])
            .ok_or(McpClientError::InvalidCatalog)?;
        let write = self.comment_tool(&tools);
        verify_account(&client, &tools, &account).await?;
        let value: Value = call_json(&client, ISSUE_READ, issue_args(&target, "get")).await?;
        let issue: Issue = match self.platform {
            IssuePlatform::Gitea => {
                serde_json::from_value(value).map_err(|_| McpClientError::Unavailable)?
            }
            IssuePlatform::Github => github::issue(value, &original)?,
        };
        if issue.id != target.issue_id
            || issue.number != number
            || issue.is_pull
            || issue.html_url != original.html_url
        {
            return Err(McpClientError::TargetChanged);
        }
        Ok(RemoteIssue {
            connection: self.clone(),
            client,
            target,
            issue,
            account_login: account.login,
            read_signature: signature(read)?,
            comment_signature: write.map(signature).transpose()?,
        })
    }

    fn comment_tool<'a>(&self, tools: &'a [Tool]) -> Option<&'a Tool> {
        match self.platform {
            IssuePlatform::Gitea => {
                compatible_tool(tools, ISSUE_WRITE, COMMENT_ARGS, &["add_comment"])
            }
            IssuePlatform::Github => {
                compatible_tool(tools, github::ADD_COMMENT, github::COMMENT_ARGS, &[])
            }
        }
    }
}

pub struct RemoteIssue {
    connection: IssueConnection,
    client: McpClient,
    target: IssueTarget,
    issue: Issue,
    account_login: String,
    read_signature: String,
    comment_signature: Option<String>,
}

impl RemoteIssue {
    pub fn prepare_reply(
        self,
        artifact: &desk_diagnose_core::issue_reply::FrozenIssueReply,
    ) -> Result<PreparedReply, McpClientError> {
        if self.target != artifact.access.target {
            return Err(McpClientError::TargetChanged);
        }
        let body = artifact.wire_body();
        self.validate_comment(&body, &artifact.comment_tool_signature)?;
        Ok(PreparedReply { issue: self, body })
    }
    /// Called by the host after its durable dispatch intent commits. The model
    /// cannot supply alternate text or an alternate destination to this method.
    pub async fn deliver_reply(
        &self,
        artifact: &desk_diagnose_core::issue_reply::FrozenIssueReply,
    ) -> Result<desk_diagnose_core::issue_reply::IssueReplyReceipt, McpClientError> {
        if self.target != artifact.access.target {
            return Err(McpClientError::TargetChanged);
        }
        let comment = self
            .add_comment(&artifact.wire_body(), &artifact.comment_tool_signature)
            .await?;
        Ok(desk_diagnose_core::issue_reply::IssueReplyReceipt {
            comment_id: comment.id,
            account_id: self.target.account_id,
            body: comment.body,
        })
    }

    pub fn issue(&self) -> &Issue {
        &self.issue
    }

    pub fn read_signature(&self) -> &str {
        &self.read_signature
    }

    pub fn comment_signature(&self) -> Option<&str> {
        self.comment_signature.as_deref()
    }

    pub async fn comments(&self) -> Result<Vec<Comment>, McpClientError> {
        let value = call_json(
            &self.client,
            ISSUE_READ,
            issue_args(&self.target, "get_comments"),
        )
        .await?;
        match self.target.platform {
            IssuePlatform::Gitea => {
                serde_json::from_value(value).map_err(|_| McpClientError::Unavailable)
            }
            IssuePlatform::Github => github::comments(value),
        }
    }

    /// Only the host's durable delivery action supplies the frozen body and signature.
    pub async fn add_comment(
        &self,
        body: &str,
        expected_signature: &str,
    ) -> Result<Comment, McpClientError> {
        self.validate_comment(body, expected_signature)?;
        self.post_comment(body).await
    }

    fn validate_comment(&self, body: &str, expected_signature: &str) -> Result<(), McpClientError> {
        if self.comment_signature() != Some(expected_signature)
            || body.is_empty()
            || body.len() > 64 * 1024
        {
            return Err(McpClientError::InvalidCatalog);
        }
        Ok(())
    }

    async fn post_comment(&self, body: &str) -> Result<Comment, McpClientError> {
        let mut args = issue_args(&self.target, "add_comment");
        args.insert("body".into(), json!(body));
        let comment: Comment = match self.target.platform {
            IssuePlatform::Gitea => call_json(&self.client, ISSUE_WRITE, args).await?,
            IssuePlatform::Github => {
                args.remove("method");
                let receipt = call_json(&self.client, github::ADD_COMMENT, args).await?;
                github::verify_comment(&self.connection, &self.target, body, receipt).await?
            }
        };
        if comment.id == 0 || comment.user != self.account_login || comment.body != body {
            return Err(McpClientError::Unavailable);
        }
        Ok(comment)
    }
}

async fn verify_account(
    client: &McpClient,
    tools: &[Tool],
    account: &Account,
) -> Result<(), McpClientError> {
    let identity =
        compatible_tool(tools, GET_ME, &[], &[]).ok_or(McpClientError::InvalidCatalog)?;
    let mcp_account: Account = call_json(client, &identity.name, JsonObject::new()).await?;
    if account.id == 0 || mcp_account.id != account.id || mcp_account.login != account.login {
        return Err(McpClientError::TargetChanged);
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn valid_account(login: &str) -> bool {
    valid_name(login.strip_suffix("[bot]").unwrap_or(login))
}

fn issue_args(target: &IssueTarget, method: &str) -> JsonObject {
    JsonObject::from_iter([
        ("method".into(), json!(method)),
        ("owner".into(), json!(target.owner)),
        ("repo".into(), json!(target.repository)),
        ("issue_number".into(), json!(target.issue_number)),
    ])
}

const ISSUE_ARGS: &[(&str, &str)] = &[
    ("method", "string"),
    ("owner", "string"),
    ("repo", "string"),
    ("issue_number", "number"),
];
const COMMENT_ARGS: &[(&str, &str)] = &[
    ("method", "string"),
    ("owner", "string"),
    ("repo", "string"),
    ("issue_number", "number"),
    ("body", "string"),
];

fn compatible_tool<'a>(
    tools: &'a [Tool],
    name: &str,
    params: &[(&str, &str)],
    methods: &[&str],
) -> Option<&'a Tool> {
    let tool = tools.iter().find(|tool| tool.name == name)?;
    let schema = &tool.input_schema;
    if schema.get("type")?.as_str()? != "object" {
        return None;
    }
    if let Some(required) = schema.get("required")
        && !required.as_array()?.iter().all(|value| {
            value
                .as_str()
                .is_some_and(|key| params.iter().any(|(supplied, _)| key == *supplied))
        })
    {
        return None;
    }
    if !params.is_empty() {
        let properties = schema.get("properties")?.as_object()?;
        for (key, kind) in params {
            let advertised = properties.get(*key)?.get("type")?.as_str()?;
            if advertised != *kind && !(*kind == "number" && advertised == "integer") {
                return None;
            }
        }
        if !methods.is_empty() {
            let advertised = properties.get("method")?.get("enum")?.as_array()?;
            if !methods.iter().all(|method| {
                advertised
                    .iter()
                    .any(|value| value.as_str() == Some(*method))
            }) {
                return None;
            }
        }
    }
    Some(tool)
}

fn signature(tool: &Tool) -> Result<String, McpClientError> {
    let schema = desk_diagnose_core::permission_tools::canonical_permission_input_json(json!({
        "name": tool.name,
        "input_schema": tool.input_schema,
    }))
    .map_err(|_| McpClientError::InvalidCatalog)?;
    Ok(format!("{:x}", Sha256::digest(schema)))
}

async fn call_json<T: DeserializeOwned>(
    client: &McpClient,
    name: &str,
    args: JsonObject,
) -> Result<T, McpClientError> {
    let result = client.call_tool(name.into(), args).await?;
    if let Some(value) = result
        .get("structuredContent")
        .filter(|value| !value.is_null())
    {
        return serde_json::from_value(value.clone()).map_err(|_| McpClientError::Unavailable);
    }
    let [text] = result
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or(McpClientError::Unavailable)?
    else {
        return Err(McpClientError::Unavailable);
    };
    if text["type"].as_str() != Some("text") {
        return Err(McpClientError::Unavailable);
    }
    serde_json::from_str(text["text"].as_str().ok_or(McpClientError::Unavailable)?)
        .map_err(|_| McpClientError::Unavailable)
}

#[cfg(test)]
mod tests;
