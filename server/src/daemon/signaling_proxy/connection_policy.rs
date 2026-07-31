//! Proxy connection outcomes, URL policy, and remote-access acknowledgements.

use super::*;

/// Outcome of handling one inbound signaling frame on a proxy link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum InboundOutcome {
    /// Keep the connection running.
    Continue,
    /// A fatal registration rejection arrived on the manager link: stop the
    /// connection and its auto-reconnect loop. Carries the error code and message.
    FatalReject { error_code: i32, message: String },
}

/// Outcome of one `maintain_proxy_connection` lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ProxyConnectionOutcome {
    /// The connection ended normally (closed / errored); the caller may reconnect.
    Closed,
    /// The manager fatally rejected registration; the caller must NOT auto-reconnect
    /// until a manual retry is requested.
    FatalReject { error_code: i32, message: String },
    /// The credential is reversibly disabled. The caller retries the same token
    /// on a slow bounded lane; it must not park forever or reissue the token.
    CredentialSuspended { error_code: i32, message: String },
    /// The host-local credential lease expired. Use a wider first-redial jitter
    /// so a shared backend outage cannot synchronize a fleet onto handshakes.
    CredentialExpired,
}

pub(super) fn manager_host_reconnect_delay(attempt: u32, jitter_ms: u64) -> Duration {
    let core_seconds = 5_u64
        .saturating_mul(1_u64.checked_shl(attempt.min(30)).unwrap_or(u64::MAX))
        .min(60);
    Duration::from_secs(core_seconds) + Duration::from_millis(jitter_ms.min(20_000))
}

pub(super) fn suspended_recovery_delay(attempt: u32, jitter_ms: u64) -> Duration {
    let core_seconds = 60_u64
        .saturating_mul(1_u64.checked_shl(attempt.min(30)).unwrap_or(u64::MAX))
        .min(300);
    Duration::from_secs(core_seconds) + Duration::from_millis(jitter_ms.min(30_000))
}

pub(super) fn next_suspended_recovery_delay(attempt: &mut u32, sample: u64) -> Duration {
    let delay = suspended_recovery_delay(*attempt, suspended_recovery_jitter_ms(sample));
    *attempt = attempt.saturating_add(1);
    delay
}

pub(super) fn manager_reconnect_jitter_ms(sample: u64) -> u64 {
    sample % 20_001
}

pub(super) fn suspended_recovery_jitter_ms(sample: u64) -> u64 {
    sample % 30_001
}

pub(super) fn lease_expiry_reconnect_jitter_ms(sample: u64) -> u64 {
    sample % 60_001
}

pub(super) fn credential_expiry_reconnect_delay(jitter_ms: u64) -> Duration {
    Duration::from_secs(5) + Duration::from_millis(jitter_ms.min(60_000))
}

pub(super) fn accelerated_probe_phase(sample: u64) -> Duration {
    Duration::from_secs(1 + sample % 10)
}

/// Whether an inbound `Error(-1)` frame is a fatal registration rejection the host
/// must stop reconnecting on. The fatal set is exactly the device-quota codes
/// (`DEVICE_QUOTA_EXCEEDED` / `DEVICE_CLIENT_ID_REQUIRED`); any other error code is
/// transient and handled normally.
pub(super) fn fatal_registration_reject(model: &SignalingModel) -> Option<(i32, String)> {
    if model.signaling_type != SignalingType::Error {
        return None;
    }
    let state = model.response_state.as_ref()?;
    let code = state.error_code;
    if code == DeskErrorCode::DEVICE_QUOTA_EXCEEDED.code()
        || code == DeskErrorCode::DEVICE_CLIENT_ID_REQUIRED.code()
    {
        let message = state
            .message
            .clone()
            .unwrap_or_else(|| "device registration rejected".to_string());
        Some((code, message))
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
/// Whether a signaling URL uses a TLS scheme (`wss` / `https`). Anything else
/// (`ws` / `http` / malformed) is treated as plaintext, so the transport guard
/// fails closed toward requiring TLS for a public target.
pub(super) fn signaling_scheme_is_tls(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("wss://") || lower.starts_with("https://")
}

/// Normalize a signaling URL exactly as the dial does, then guard the (possibly
/// IP-literal) target before connecting. Returns the cleaned URL to dial, or an
/// error when the transport policy refuses it.
///
/// The dial strips control characters, and `char::is_control` covers code points
/// (e.g. U+007F) that URL parsing does not — so guarding the *raw* string would let
/// a control-prefixed literal fail the parse (and be deferred as if it were a
/// domain) yet clean up into a valid IP-literal dial, re-opening the metadata floor
/// and the public-plaintext refusal for literals. Cleaning first and guarding the
/// exact string that is dialed closes that mismatch. The actix-tls resolver
/// short-circuits an IP literal before the custom guard resolver runs, which is why
/// a literal must be judged here rather than only in the resolver.
pub(super) fn guard_and_clean_signaling_url(
    signaling_url: &str,
    require_secure_signaling: bool,
) -> Result<String, String> {
    let url_clean = signaling_url.trim().trim_matches(|c: char| c.is_control());
    // Drop any fragment: a dial URL has none, and the auth token is appended as a
    // `?token=...` query below — after a fragment that query would both fail to
    // reach the server-side token read and land inside the fragment (defeating log
    // redaction). Stripping it keeps the token in a proper query.
    let url_clean = url_clean.split('#').next().unwrap_or(url_clean);
    let scheme_is_tls = signaling_scheme_is_tls(url_clean);
    desk_utils::ssrf::check_transport_for_url(
        url_clean,
        true,
        scheme_is_tls,
        require_secure_signaling,
    )
    .map_err(|e| format!("signaling target refused: {e}"))?;
    Ok(url_clean.to_string())
}

/// Render a dial URL for logging with every credential-bearing part neutralized:
/// the `token` query value is masked, any userinfo (`user:pass@`) is stripped, and
/// any fragment is dropped (a malformed base could otherwise push the appended
/// token into the fragment). If the URL does not parse, fail safe by keeping only
/// the part before the first `?`/`#` (a credential can only live in the query or
/// fragment) rather than logging it verbatim.
pub(super) fn redact_token_in_url(raw: &str) -> String {
    let Ok(mut url) = url::Url::parse(raw) else {
        // Unparseable: any part of the raw string may carry a credential (userinfo,
        // query, or fragment) and we have no parser to isolate the safe parts, so log
        // a fixed placeholder — nothing from `raw` is emitted. An unparseable URL
        // would not have dialed anyway (awc's `http::Uri` parser rejects it too), so
        // no useful debugging information is lost.
        return "<unparseable url>".to_string();
    };
    // Userinfo may carry credentials; strip it. A fragment is never dialed and could
    // carry an appended token, so drop it.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_fragment(None);
    if url.query().is_some() {
        let redacted: Vec<(String, String)> = url
            .query_pairs()
            .map(|(k, v)| {
                if k.eq_ignore_ascii_case("token") {
                    (k.into_owned(), "***".to_string())
                } else {
                    (k.into_owned(), v.into_owned())
                }
            })
            .collect();
        {
            let mut qs = url.query_pairs_mut();
            qs.clear();
            for (k, v) in &redacted {
                qs.append_pair(k, v);
            }
        }
    }
    url.to_string()
}

pub(super) fn pending_remote_access_frame(hub: &HostControlHub) -> Option<String> {
    let coordinator = hub.remote_access_coordinator()?;
    let request = coordinator.pending_central_request()?;
    let data = serde_json::to_value(&request).ok()?;
    serde_json::to_string(&SignalingModel::new(
        &request.request_id,
        SignalingType::HostRemoteAccessLockRequest,
        None,
        None,
        Some(data),
        None,
    ))
    .ok()
}

pub(super) async fn consume_remote_access_ack(text: &str, hub: &HostControlHub) -> bool {
    let Ok(model) = serde_json::from_str::<SignalingModel>(text) else {
        return false;
    };
    match model.signaling_type {
        SignalingType::HostRemoteAccessLockAck => {
            let Ok(ack) = model
                .get_data::<desk_signal_facade::model::remote_access::HostRemoteAccessLockAck>()
            else {
                warn!("[remote-access] malformed central lock ack dropped");
                return true;
            };
            if ack.request_id != model.request_id {
                warn!("[remote-access] central lock ack request id mismatch");
                return true;
            }
            if let Some(coordinator) = hub.remote_access_coordinator() {
                match coordinator.acknowledge_central(&ack).await {
                    Ok(true) => info!(
                        "[remote-access] central mirror acknowledged version {}",
                        ack.state_version
                    ),
                    Ok(false) => warn!(
                        "[remote-access] stale or mismatched central ack ignored (version {})",
                        ack.state_version
                    ),
                    Err(error) => {
                        error!("[remote-access] could not persist central ack: {error:#}")
                    }
                }
            }
            true
        }
        SignalingType::TerminateRemotePeerAck => {
            if let Ok(ack) =
                model.get_data::<desk_signal_facade::model::remote_access::TerminateRemotePeerAck>()
            {
                info!(
                    "[remote-access] peer eviction {} for {}: {:?}",
                    ack.operation_id, ack.target_connection_id, ack.outcome
                );
            }
            true
        }
        _ => false,
    }
}
