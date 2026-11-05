//! REST source reads do not depend on the MCP server being available.

use super::*;
use desk_diagnose_core::issue_authority::IssueRepositoryScope;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryComment {
    pub comment: RestComment,
    /// Gitea exposes a separate PR URL; those comments never become issue work.
    pub issue_number: Option<u64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub automation: bool,
}

impl RepositoryComment {
    pub fn freeze(
        self,
        repository: &IssueRepositoryScope,
    ) -> Result<desk_diagnose_core::issue_events::IssueSourceComment, McpClientError> {
        let sender_id = self
            .comment
            .user
            .as_ref()
            .filter(|user| user.id > 0 && valid_account(&user.login))
            .map(|user| user.id);
        let comment = desk_diagnose_core::issue_events::IssueSourceComment {
            repository: repository.clone(),
            comment_id: self.comment.id,
            issue_number: self.issue_number,
            sender_id,
            automation: self.automation,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            body: self.comment.body,
        };
        comment
            .validate()
            .map_err(|_| McpClientError::Unavailable)?;
        Ok(comment)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryCommentPage {
    pub comments: Vec<RepositoryComment>,
    pub next_page: Option<std::num::NonZeroU32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestIssueSnapshot {
    pub target: IssueTarget,
    pub title: String,
    pub body: String,
}

impl IssueConnection {
    /// Bind a user-selected issue locator using only identities returned by the forge.
    pub async fn bind_subscription(
        &self,
        connection_ref: &str,
        owner: &str,
        repository: &str,
        issue_number: Option<u64>,
    ) -> Result<
        (
            IssueRepositoryScope,
            Option<desk_diagnose_core::issue_authority::IssueFilter>,
        ),
        McpClientError,
    > {
        let scope = self
            .bind_repository(connection_ref, owner, repository)
            .await?;
        let filter = if let Some(number) = issue_number {
            let issue = self
                .read_issue_snapshot(&scope, number)
                .await?
                .ok_or(McpClientError::TargetChanged)?;
            Some(desk_diagnose_core::issue_authority::IssueFilter {
                issue_id: issue.target.issue_id,
                issue_number: issue.target.issue_number,
            })
        } else {
            None
        };
        Ok((scope, filter))
    }

    pub async fn bind_repository(
        &self,
        connection_ref: &str,
        owner: &str,
        repository: &str,
    ) -> Result<IssueRepositoryScope, McpClientError> {
        if connection_ref.is_empty()
            || connection_ref.len() > 512
            || connection_ref.chars().any(char::is_control)
        {
            return Err(McpClientError::InvalidConfiguration);
        }
        Ok(self
            .resolve_repository(connection_ref, owner, repository)
            .await?
            .0)
    }

    pub(super) async fn resolve_repository(
        &self,
        connection_ref: &str,
        owner: &str,
        repository: &str,
    ) -> Result<(IssueRepositoryScope, Account), McpClientError> {
        if !valid_name(owner) || !valid_name(repository) {
            return Err(McpClientError::InvalidConfiguration);
        }
        let account_path = ["user"];
        let repository_path = ["repos", owner, repository];
        let (account, repo): (Account, Repository) =
            tokio::try_join!(self.get(&account_path), self.get(&repository_path))?;
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
            || repo
                .visibility
                .as_deref()
                .is_some_and(|value| !matches!(value, "public" | "private" | "internal"))
        {
            return Err(McpClientError::TargetChanged);
        }
        Ok((
            IssueRepositoryScope {
                platform: self.platform,
                connection_ref: connection_ref.into(),
                instance: self.instance.to_string(),
                account_id: account.id,
                repository_id: repo.id,
                owner: canonical_owner.into(),
                repository: canonical_repo.into(),
                private: repo.private,
                internal: repo.internal || repo.visibility.as_deref() == Some("internal"),
            },
            account,
        ))
    }

    async fn check_repository(
        &self,
        expected: &IssueRepositoryScope,
    ) -> Result<(), McpClientError> {
        if expected.platform != self.platform
            || expected.instance != self.instance.as_str()
            || self
                .bind_repository(
                    &expected.connection_ref,
                    &expected.owner,
                    &expected.repository,
                )
                .await?
                != *expected
        {
            return Err(McpClientError::TargetChanged);
        }
        Ok(())
    }

    pub async fn read_issue_snapshot(
        &self,
        expected: &IssueRepositoryScope,
        number: u64,
    ) -> Result<Option<RestIssueSnapshot>, McpClientError> {
        if number == 0 || number > i64::MAX as u64 {
            return Err(McpClientError::InvalidConfiguration);
        }
        self.check_repository(expected).await?;
        #[derive(Deserialize)]
        struct FullIssue {
            #[serde(flatten)]
            identity: RestIssue,
            title: String,
            body: Option<String>,
        }
        let issue: FullIssue = self
            .get(&[
                "repos",
                &expected.owner,
                &expected.repository,
                "issues",
                &number.to_string(),
            ])
            .await?;
        if issue.identity.id == 0 || issue.identity.number != number || issue.title.is_empty() {
            return Err(McpClientError::TargetChanged);
        }
        if issue.identity.pull_request.is_some() {
            return Ok(None);
        }
        // Returned URLs are identities only; they are never followed with credentials.
        if issue.identity.html_url != self.issue_web_url(expected, number)?.as_str() {
            return Err(McpClientError::TargetChanged);
        }
        Ok(Some(RestIssueSnapshot {
            target: IssueTarget {
                platform: expected.platform,
                instance: expected.instance.clone(),
                account_id: expected.account_id,
                repository_id: expected.repository_id,
                owner: expected.owner.clone(),
                repository: expected.repository.clone(),
                issue_number: number,
                issue_id: issue.identity.id,
                private: expected.private,
                internal: expected.internal,
            },
            title: issue.title,
            body: issue.body.unwrap_or_default(),
        }))
    }

    /// The lower bound deliberately has no upper time filter. Editing an older
    /// row must not remove it ahead of an offset page and hide a later comment.
    pub async fn repository_comments_page(
        &self,
        expected: &IssueRepositoryScope,
        since_ms: i64,
        page: std::num::NonZeroU32,
    ) -> Result<RepositoryCommentPage, McpClientError> {
        let since = chrono::DateTime::from_timestamp_millis(since_ms)
            .filter(|_| since_ms >= 0)
            .ok_or(McpClientError::InvalidConfiguration)?
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        self.check_repository(expected).await?;
        let mut url = self.rest_url(&[
            "repos",
            &expected.owner,
            &expected.repository,
            "issues",
            "comments",
        ])?;
        url.query_pairs_mut()
            .append_pair("since", &since)
            .append_pair("page", &page.to_string())
            .append_pair(
                match self.platform {
                    IssuePlatform::Gitea => "limit",
                    IssuePlatform::Github => "per_page",
                },
                &COMMENT_PAGE_SIZE.to_string(),
            );
        if self.platform == IssuePlatform::Github {
            url.query_pairs_mut()
                .append_pair("sort", "created")
                .append_pair("direction", "asc");
        }
        let comments: Vec<RestComment> = self
            .mcp
            .client
            .get_json(url, &self.mcp.bearer_token)
            .await?;
        if comments.len() > COMMENT_PAGE_SIZE {
            return Err(McpClientError::Unavailable);
        }
        let next_page = if comments.is_empty() {
            None
        } else {
            Some(
                page.get()
                    .checked_add(1)
                    .and_then(std::num::NonZeroU32::new)
                    .ok_or(McpClientError::Unavailable)?,
            )
        };
        let comments = comments
            .into_iter()
            .map(|comment| {
                let created_at_ms = timestamp(&comment.created_at)?;
                let updated_at_ms = timestamp(&comment.updated_at)?;
                if comment.id == 0 || updated_at_ms < created_at_ms {
                    return Err(McpClientError::Unavailable);
                }
                let issue_number = if !comment.pull_request_url.is_empty() {
                    None
                } else {
                    Some(self.comment_issue_number(expected, &comment.issue_url)?)
                };
                let automation = self.platform == IssuePlatform::Github
                    && (comment.performed_via_github_app.is_some()
                        || comment
                            .user
                            .as_ref()
                            .and_then(|user| user.account_type.as_deref())
                            != Some("User"));
                Ok(RepositoryComment {
                    comment,
                    issue_number,
                    created_at_ms,
                    updated_at_ms,
                    automation,
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(RepositoryCommentPage {
            comments,
            next_page,
        })
    }

    fn issue_web_url(
        &self,
        repository: &IssueRepositoryScope,
        number: u64,
    ) -> Result<Url, McpClientError> {
        let mut url = self.instance.clone();
        url.path_segments_mut()
            .map_err(|_| McpClientError::InvalidConfiguration)?
            .pop_if_empty()
            .extend([
                &repository.owner,
                &repository.repository,
                "issues",
                &number.to_string(),
            ]);
        Ok(url)
    }

    pub(super) fn comment_issue_number(
        &self,
        repository: &IssueRepositoryScope,
        value: &str,
    ) -> Result<u64, McpClientError> {
        let mut prefix = match self.platform {
            IssuePlatform::Gitea => self.issue_web_url(repository, 1)?,
            IssuePlatform::Github => self.rest_url(&[
                "repos",
                &repository.owner,
                &repository.repository,
                "issues",
                "1",
            ])?,
        };
        prefix
            .path_segments_mut()
            .map_err(|_| McpClientError::InvalidConfiguration)?
            .pop();
        let number = value
            .strip_prefix(prefix.as_str())
            .and_then(|value| value.strip_prefix('/'))
            .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|number| *number > 0 && *number <= i64::MAX as u64)
            .ok_or(McpClientError::TargetChanged)?;
        Ok(number)
    }
}

fn timestamp(value: &str) -> Result<i64, McpClientError> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|time| time.timestamp_millis())
        .ok()
        .filter(|time| *time >= 0)
        .ok_or(McpClientError::Unavailable)
}
