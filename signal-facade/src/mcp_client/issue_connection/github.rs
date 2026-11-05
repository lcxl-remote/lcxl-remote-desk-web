//! Normalize the official GitHub MCP legacy and typed issue responses.

use super::*;

pub(super) const ADD_COMMENT: &str = "add_issue_comment";
pub(super) const COMMENT_ARGS: &[(&str, &str)] = &[
    ("owner", "string"),
    ("repo", "string"),
    ("issue_number", "number"),
    ("body", "string"),
];

fn payload(value: Value, method: &str, key: &str) -> Result<Value, McpClientError> {
    if let Some(actual) = value.get("method") {
        if actual != method {
            return Err(McpClientError::Unavailable);
        }
        value.get(key).cloned().ok_or(McpClientError::Unavailable)
    } else {
        Ok(value)
    }
}

pub(super) fn issue(value: Value, original: &RestIssue) -> Result<Issue, McpClientError> {
    #[derive(Deserialize)]
    struct MinimalIssue {
        number: u64,
        title: String,
        #[serde(default)]
        body: String,
        html_url: String,
    }
    let minimal: MinimalIssue = serde_json::from_value(payload(value, "get", "issue")?)
        .map_err(|_| McpClientError::Unavailable)?;
    if minimal.number != original.number || minimal.html_url != original.html_url {
        return Err(McpClientError::TargetChanged);
    }
    // GitHub MCP omits the database ID. Only the verified REST target supplies it.
    Ok(Issue {
        id: original.id,
        number: minimal.number,
        title: minimal.title,
        body: minimal.body,
        html_url: minimal.html_url,
        is_pull: false,
    })
}

pub(super) fn comments(value: Value) -> Result<Vec<Comment>, McpClientError> {
    #[derive(Deserialize)]
    struct MinimalComment {
        id: u64,
        #[serde(default)]
        body: String,
        user: Account,
        #[serde(default)]
        created_at: String,
    }
    let comments: Vec<MinimalComment> =
        serde_json::from_value(payload(value, "get_comments", "comments")?)
            .map_err(|_| McpClientError::Unavailable)?;
    comments
        .into_iter()
        .map(|comment| {
            if comment.id == 0 || comment.user.id == 0 || !valid_account(&comment.user.login) {
                return Err(McpClientError::Unavailable);
            }
            Ok(Comment {
                id: comment.id,
                body: comment.body,
                user: comment.user.login,
                created_at: comment.created_at,
            })
        })
        .collect()
}

pub(super) async fn verify_comment(
    connection: &IssueConnection,
    target: &IssueTarget,
    body: &str,
    receipt: Value,
) -> Result<Comment, McpClientError> {
    let id = receipt
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|id| id.parse::<u64>().ok())
        .filter(|id| *id > 0)
        .ok_or(McpClientError::Unavailable)?;
    #[derive(Deserialize)]
    struct PublishedComment {
        id: u64,
        body: String,
        user: Account,
        created_at: String,
        issue_url: String,
    }
    let comment: PublishedComment = connection
        .get(&[
            "repos",
            &target.owner,
            &target.repository,
            "issues",
            "comments",
            &id.to_string(),
        ])
        .await?;
    let issue_url = connection.rest_url(&[
        "repos",
        &target.owner,
        &target.repository,
        "issues",
        &target.issue_number.to_string(),
    ])?;
    if comment.id != id
        || comment.body != body
        || comment.user.id != target.account_id
        || comment.issue_url != issue_url.as_str()
    {
        return Err(McpClientError::Unavailable);
    }
    Ok(Comment {
        id,
        body: comment.body,
        user: comment.user.login,
        created_at: comment.created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_and_typed_issues_share_the_rest_identity_without_fabricating_mcp_ids() {
        let original = RestIssue {
            id: 82,
            number: 4,
            html_url: "https://github.com/Org/Project/issues/4".into(),
            pull_request: None,
        };
        let value = json!({"number":4,"title":"macOS problem","html_url":original.html_url});
        let legacy = issue(value.clone(), &original).unwrap();
        assert_eq!(legacy.id, 82);
        assert!(legacy.body.is_empty());
        assert_eq!(
            legacy,
            issue(json!({"method":"get","issue":value}), &original).unwrap()
        );
        assert!(issue(json!({"method":"get_comments","issue":{}}), &original).is_err());
        assert!(
            issue(
                json!({"number":5,"title":"other","html_url":original.html_url}),
                &original
            )
            .is_err()
        );
    }

    #[test]
    fn comments_require_original_author_identity_and_accept_both_wire_formats() {
        let value = json!([{"id":90,"body":"Steps","user":{"id":7,"login":"reporter"}}]);
        let legacy = comments(value.clone()).unwrap();
        assert_eq!(legacy[0].user, "reporter");
        assert_eq!(
            legacy,
            comments(json!({"method":"get_comments","comments":value})).unwrap()
        );
        assert!(comments(json!([{"id":90,"user":"reporter"}])).is_err());
        assert!(comments(json!([{"id":90,"user":{"id":0,"login":"reporter"}}])).is_err());
    }
}
