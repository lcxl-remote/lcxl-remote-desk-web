//! `container.*` collectors — Docker containers via the Engine API.
//!
//! Talks to the local Docker daemon (Windows named pipe / Unix socket) with
//! bollard. Docker is optional: when the daemon is absent or unreachable the
//! collectors degrade silently to `UnsupportedCapability`,
//! never failing the transport. A missing container id maps to `InvalidInput`.
//!
//! These collectors are async (bollard is async-native), so the dispatch awaits
//! them directly rather than running them on the blocking pool.

use std::time::{SystemTime, UNIX_EPOCH};

use bollard::Docker;
use bollard::container::LogOutput;
use bollard::models::ContainerSummaryStateEnum;
use bollard::query_parameters::{
    InspectContainerOptions, ListContainersOptionsBuilder, LogsOptionsBuilder,
};
use desk_agent_protocol::{
    AgentError, AgentErrorKind, ContainerInspectOutput, ContainerInspectParams,
    ContainerListOutput, ContainerListParams, ContainerLogsOutput, ContainerLogsParams,
    ContainerSummary,
};
use futures::StreamExt;

/// Cap on listed containers; more sets `truncated`.
const MAX_CONTAINERS: usize = 200;
/// Returned log lines when the caller does not specify a limit.
const DEFAULT_LOG_LINES: u32 = 200;
/// Hard ceiling on returned log lines.
const MAX_LOG_LINES: u32 = 1000;
/// Byte ceiling on the inspect JSON document; more sets `truncated`.
const MAX_INSPECT_BYTES: usize = 128 * 1024;

/// Connect to the local Docker daemon and confirm it answers. Any
/// connection/ping failure means Docker is unavailable → silent degrade.
async fn connect() -> Result<Docker, AgentError> {
    let docker = Docker::connect_with_local_defaults().map_err(|_| unavailable())?;
    docker.ping().await.map_err(|_| unavailable())?;
    Ok(docker)
}

fn unavailable() -> AgentError {
    AgentError {
        kind: AgentErrorKind::UnsupportedCapability,
        message: "Docker is not available on this host".to_string(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

/// Map a Docker API error after a successful ping: a 404 is a bad container id
/// (caller input), anything else is an internal/transport failure.
fn docker_call_err(err: bollard::errors::Error) -> AgentError {
    if let bollard::errors::Error::DockerResponseServerError {
        status_code: 404,
        message,
    } = &err
    {
        return AgentError {
            kind: AgentErrorKind::InvalidInput,
            message: format!("container not found: {message}"),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        };
    }
    AgentError {
        kind: AgentErrorKind::Internal,
        message: format!("Docker API call failed: {err}"),
        retryable: true,
        safe_for_model: true,
        error_code: None,
    }
}

/// Docker prefixes container names with `/`; take the first and strip it.
fn clean_name(names: Option<Vec<String>>) -> String {
    names
        .and_then(|names| names.into_iter().next())
        .map(|name| name.trim_start_matches('/').to_string())
        .unwrap_or_default()
}

fn state_string(state: Option<ContainerSummaryStateEnum>) -> String {
    state.map(|s| s.to_string()).unwrap_or_default()
}

/// Truncate `s` to at most `max` bytes without splitting a UTF-8 character.
/// `String::truncate` panics if the index is not on a char boundary, and
/// inspect JSON can contain non-ASCII (names, labels, mount paths), so back off
/// to the nearest boundary at or below `max`.
fn truncate_on_char_boundary(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

pub async fn list(_params: &ContainerListParams) -> Result<ContainerListOutput, AgentError> {
    let docker = connect().await?;
    let options = ListContainersOptionsBuilder::new().all(true).build();
    let summaries = docker
        .list_containers(Some(options))
        .await
        .map_err(docker_call_err)?;

    let mut containers: Vec<ContainerSummary> = summaries
        .into_iter()
        .map(|c| ContainerSummary {
            id: c.id.unwrap_or_default(),
            name: clean_name(c.names),
            image: c.image.unwrap_or_default(),
            state: state_string(c.state),
        })
        .collect();
    let truncated = containers.len() > MAX_CONTAINERS;
    containers.truncate(MAX_CONTAINERS);
    Ok(ContainerListOutput {
        containers,
        truncated,
    })
}

pub async fn inspect(
    params: &ContainerInspectParams,
) -> Result<ContainerInspectOutput, AgentError> {
    let docker = connect().await?;
    let response = docker
        .inspect_container(&params.container_id, None::<InspectContainerOptions>)
        .await
        .map_err(docker_call_err)?;

    let mut details_json = serde_json::to_string(&response).map_err(|e| AgentError {
        kind: AgentErrorKind::Internal,
        message: format!("failed to serialize inspect response: {e}"),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    })?;
    // Truncate by size if the document is oversized; the result is evidence
    // text, not required to stay valid JSON when truncated.
    let truncated = details_json.len() > MAX_INSPECT_BYTES;
    if truncated {
        truncate_on_char_boundary(&mut details_json, MAX_INSPECT_BYTES);
    }

    Ok(ContainerInspectOutput {
        container_id: params.container_id.clone(),
        details_json,
        // No scrubbing pass yet, so no redaction markers are reported.
        redactions: Vec::new(),
        truncated,
    })
}

pub async fn logs(params: &ContainerLogsParams) -> Result<ContainerLogsOutput, AgentError> {
    let docker = connect().await?;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_LOG_LINES)
        .clamp(1, MAX_LOG_LINES);

    let mut builder = LogsOptionsBuilder::new()
        .stdout(true)
        .stderr(true)
        .timestamps(true)
        .tail(&limit.to_string());
    if let Some(since_minutes) = params.since_minutes {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let since = now.saturating_sub(u64::from(since_minutes) * 60);
        builder = builder.since(since as i32);
    }

    let mut stream = docker.logs(&params.container_id, Some(builder.build()));
    let mut buffer = String::new();
    while let Some(item) = stream.next().await {
        let output = item.map_err(docker_call_err)?;
        let (LogOutput::StdErr { message }
        | LogOutput::StdOut { message }
        | LogOutput::StdIn { message }
        | LogOutput::Console { message }) = output;
        buffer.push_str(&String::from_utf8_lossy(&message));
    }

    let mut lines: Vec<String> = buffer.lines().map(|line| line.to_string()).collect();
    let truncated = lines.len() as u32 > limit;
    lines.truncate(limit as usize);
    Ok(ContainerLogsOutput {
        lines,
        // Redaction pipeline lands in M1b.
        redactions: Vec::new(),
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_name_strips_leading_slash() {
        assert_eq!(
            clean_name(Some(vec!["/web".to_string(), "/web2".to_string()])),
            "web"
        );
        assert_eq!(clean_name(Some(vec![])), "");
        assert_eq!(clean_name(None), "");
    }

    #[test]
    fn truncate_on_char_boundary_never_splits_a_char() {
        // "é" is 2 bytes (0xC3 0xA9). A byte limit landing inside it must back
        // off to the boundary before it rather than panic.
        let mut s = "aaé".to_string(); // bytes: a a C3 A9  (len 4)
        truncate_on_char_boundary(&mut s, 3); // 3 splits 'é'
        assert_eq!(s, "aa");

        // Limit on a boundary keeps the whole char.
        let mut s2 = "aaé".to_string();
        truncate_on_char_boundary(&mut s2, 4);
        assert_eq!(s2, "aaé");

        // Under the limit is a no-op.
        let mut s3 = "abc".to_string();
        truncate_on_char_boundary(&mut s3, 100);
        assert_eq!(s3, "abc");

        // A multi-byte string truncated to a non-boundary stays valid UTF-8.
        let mut s4 = "日本語".to_string(); // 9 bytes, 3-byte chars
        truncate_on_char_boundary(&mut s4, 4); // mid second char
        assert_eq!(s4, "日");
    }

    #[test]
    fn state_string_maps_enum() {
        assert_eq!(
            state_string(Some(ContainerSummaryStateEnum::RUNNING)),
            "running"
        );
        assert_eq!(
            state_string(Some(ContainerSummaryStateEnum::EXITED)),
            "exited"
        );
        assert_eq!(state_string(None), "");
    }

    #[test]
    fn call_err_maps_404_to_invalid_input() {
        let err = docker_call_err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404,
            message: "no such container".to_string(),
        });
        assert_eq!(err.kind, AgentErrorKind::InvalidInput);

        let other = docker_call_err(bollard::errors::Error::DockerResponseServerError {
            status_code: 500,
            message: "boom".to_string(),
        });
        assert_eq!(other.kind, AgentErrorKind::Internal);
    }

    /// The collector must always degrade gracefully: on a host without Docker
    /// it returns `UnsupportedCapability`; on one with Docker it returns `Ok`.
    /// Either way it never panics or hangs the transport.
    #[tokio::test]
    async fn list_is_ok_or_unsupported() {
        match list(&ContainerListParams::default()).await {
            Ok(_) => {}
            Err(e) => assert_eq!(e.kind, AgentErrorKind::UnsupportedCapability),
        }
    }
}
