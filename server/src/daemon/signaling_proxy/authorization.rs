//! Source-aware authorization for AI, RequestRemoteAccess, and terminal frames.

use super::*;

/// Classify the daemon's local loopback signaling link by startup mode.
///
/// In portable `Default` mode the loopback reaches the **embedded signal acting
/// as the central brain** — same process, single machine, authenticated by the
/// local token — so the link is trusted-central: that signal pushes evidence
/// collection (`CollectRequest`) and wrapped AI frames over it, which the edge
/// must accept. In `ServiceDaemon` mode the loopback is the daemon's own internal
/// API, not a central brain (the real central is remote, reached through the
/// central credential slot), so it stays a plain `Local` link with no PDP.
pub(super) fn local_loopback_source(mode: &StartupMode) -> InboundSignalingSource {
    match mode {
        StartupMode::Default => InboundSignalingSource::TrustedCentral,
        _ => InboundSignalingSource::Local,
    }
}

/// Outcome of the source-gated authorization check for one inbound frame.
pub(super) enum AuthzGateOutcome {
    /// Forward this (possibly unwrapped) model to the router, carrying the
    /// validated authorization block when the frame arrived wrapped from the
    /// manager link.
    Pass(SignalingModel, Option<AuthorizationBlock>),
    /// Drop the frame; the string explains why (for logging).
    Drop(String),
}

/// True for the control-end AI frames that may carry an authorization wrapper.
pub(super) fn is_ai_control_frame(t: SignalingType) -> bool {
    matches!(
        t,
        SignalingType::InvokeAgentCapability
            | SignalingType::DiagnoseDevice
            | SignalingType::PreviewExecution
    )
}

/// Source-gate an inbound AI frame against the authorization wrapper rules
/// (security model D11/D20):
///
/// - Non-AI frames pass through untouched.
/// - A wrapper (`AuthorizedControlPayload`) is only legitimate from the
///   `TrustedCentral` link; on any other source it is dropped (a non-central
///   upstream must never inject authorization).
/// - On the `TrustedCentral` link a wrapper is validated against the frame
///   (`request_id`), this daemon's audience, and expiry; on success the inner
///   payload is unwrapped and forwarded. The carried decision is consumed by the
///   enforcement step (the policy-injection stage); here the mechanism only
///   validates and unwraps.
/// - A bare payload passes through to local-config gating.
pub(super) fn gate_authz_frame(
    model: SignalingModel,
    source: InboundSignalingSource,
    expected_audience: &str,
    now_rfc3339: &str,
) -> AuthzGateOutcome {
    if !is_ai_control_frame(model.signaling_type) {
        return AuthzGateOutcome::Pass(model, None);
    }

    let has_wrapper = model
        .get_raw_data()
        .as_ref()
        .and_then(|v| v.as_object())
        .map(|o| o.contains_key("authz") && o.contains_key("inner"))
        .unwrap_or(false);

    if !has_wrapper {
        // The trusted-central link always wraps AI control frames (its PDP
        // authorizes and wraps every one), so a bare AI control frame from the
        // trusted-central source is illegitimate — forged or a relay fault — and
        // is dropped rather than falling through to the local default scope,
        // which would bypass the central policy. Local / remote-signaling links
        // have no PDP and pass bare frames through to local-config gating.
        if source == InboundSignalingSource::TrustedCentral {
            return AuthzGateOutcome::Drop(
                "bare AI control frame from trusted-central source (authorization wrapper required)"
                    .to_string(),
            );
        }
        return AuthzGateOutcome::Pass(model, None);
    }

    if source != InboundSignalingSource::TrustedCentral {
        return AuthzGateOutcome::Drop(format!(
            "AI frame carried an authz wrapper from non-central source {source:?}"
        ));
    }

    let raw = match model.get_raw_data().clone() {
        Some(v) => v,
        None => return AuthzGateOutcome::Drop("wrapper frame had no data".to_string()),
    };
    let wrapper: AuthorizedControlPayload<serde_json::Value> = match serde_json::from_value(raw) {
        Ok(w) => w,
        Err(e) => return AuthzGateOutcome::Drop(format!("malformed authz wrapper: {e}")),
    };

    if let Err(e) = wrapper
        .authz
        .validate(&model.request_id, expected_audience, now_rfc3339)
    {
        return AuthzGateOutcome::Drop(format!("authz wrapper rejected: {e:?}"));
    }

    // Validated: forward the inner payload as a bare frame plus the validated
    // authorization block, which the router threads into the AI handlers
    // (scope / max_risk / orchestrator grants) to enforce the central decision.
    let unwrapped = SignalingModel::new(
        &model.request_id,
        model.signaling_type,
        model.from_connection_id.clone(),
        model.to_connection_id.clone(),
        Some(wrapper.inner),
        model.response_state.clone(),
    );
    AuthzGateOutcome::Pass(unwrapped, Some(wrapper.authz))
}

/// Outcome of the source-gated capability-ceiling check for a `RequestRemoteAccess`.
pub(super) enum RequestRemoteGateOutcome {
    /// Forward this (possibly unwrapped) model to the router, carrying the
    /// validated capability-ceiling stamp when the request arrived wrapped from
    /// the trusted-central link.
    Pass(SignalingModel, Option<RequestRemoteAuthz>),
    /// Drop the frame; the string explains why (for logging).
    Drop(String),
}

/// Source-gate an inbound `RequestRemoteAccess` against the capability-ceiling stamp
/// rules. This is the anti-downgrade anchor (mirrors [`gate_authz_frame`]): the
/// trusted-central link always stamps every `RequestRemoteAccess` (owner → no ceiling,
/// redeemed grant → its ceiling), so on that link a bare request is illegitimate
/// and dropped, and a stamp from any other source is an illegitimate injection
/// and dropped.
///
/// - A **wrapper** (`AuthorizedRequestRemote`) is only legitimate from
///   `TrustedCentral`; on any other source it is dropped.
/// - On `TrustedCentral` a **bare** `RequestRemoteAccess` is dropped — a forged frame,
///   a relay fault, or a grant session stripping its stamp to masquerade as an
///   owner. Dropping it here is the only defense (there is no physical restricted
///   upstream to fall back on).
/// - On `TrustedCentral` a wrapper is validated against the frame (`request_id`),
///   this daemon's audience, and expiry; on success the inner frame is unwrapped
///   and forwarded with the validated stamp (the ceiling the router / worker
///   enforce).
/// - On a non-central source (loopback / relay / support) a bare `RequestRemoteAccess`
///   passes through unchanged: the owner-only relay path, where there is no
///   central to stamp and redeemed codes are hard-rejected at redeem time.
pub(super) fn gate_request_remote_frame(
    model: SignalingModel,
    source: InboundSignalingSource,
    expected_audience: &str,
    now_rfc3339: &str,
) -> RequestRemoteGateOutcome {
    let has_wrapper = model
        .get_raw_data()
        .as_ref()
        .and_then(|v| v.as_object())
        .map(|o| o.contains_key("authz") && o.contains_key("inner"))
        .unwrap_or(false);

    if !has_wrapper {
        if source == InboundSignalingSource::TrustedCentral {
            return RequestRemoteGateOutcome::Drop(
                "bare RequestRemoteAccess from trusted-central source (capability-ceiling stamp required)"
                    .to_string(),
            );
        }
        return RequestRemoteGateOutcome::Pass(model, None);
    }

    if source != InboundSignalingSource::TrustedCentral {
        return RequestRemoteGateOutcome::Drop(format!(
            "RequestRemoteAccess carried a capability-ceiling stamp from non-central source {source:?}"
        ));
    }

    let raw = match model.get_raw_data().clone() {
        Some(v) => v,
        None => return RequestRemoteGateOutcome::Drop("stamped frame had no data".to_string()),
    };
    let wrapper: AuthorizedRequestRemote = match serde_json::from_value(raw) {
        Ok(w) => w,
        Err(e) => {
            return RequestRemoteGateOutcome::Drop(format!(
                "malformed RequestRemoteAccess stamp wrapper: {e}"
            ));
        }
    };

    if let Err(e) = wrapper
        .authz
        .validate(&model.request_id, expected_audience, now_rfc3339)
    {
        return RequestRemoteGateOutcome::Drop(format!(
            "RequestRemoteAccess stamp rejected: {e:?}"
        ));
    }

    // Validated: forward the inner frame as a bare RequestRemoteAccess plus the
    // validated stamp, which the router threads into the session's restriction /
    // capability-ceiling enforcement.
    let unwrapped = SignalingModel::new(
        &model.request_id,
        model.signaling_type,
        model.from_connection_id.clone(),
        model.to_connection_id.clone(),
        Some(wrapper.inner),
        model.response_state.clone(),
    );
    RequestRemoteGateOutcome::Pass(unwrapped, Some(wrapper.authz))
}

/// Source-gate an inbound `StartTerminal` against the capability-ceiling stamp
/// rules, the terminal analogue of [`gate_request_remote_frame`]. The remote
/// terminal opens on a distinct WS connection that never does a `RequestRemoteAccess`, so
/// `StartTerminal` is *its* admission-establishing frame and must carry the same
/// stamp discipline:
///
/// - A **wrapper** (`AuthorizedTerminalStart`) is only legitimate from
///   `TrustedCentral`; on any other source it is dropped (a non-central stamp is an
///   illegitimate injection).
/// - On `TrustedCentral` a **bare** `StartTerminal` is dropped — the central always
///   stamps (owner → no ceiling, redeemed code → its ceiling), so a bare one is a
///   forged frame or a stamp-stripping downgrade attempt.
/// - On `TrustedCentral` a wrapper is validated against the frame (`request_id`),
///   this daemon's audience, and expiry; on success the inner `StartTerminalSession`
///   is unwrapped and forwarded with the validated stamp (which
///   `handle_start_terminal_inbound` turns into the connection's ceiling + admission
///   + grant index).
/// - On a non-central source (loopback / relay) a bare `StartTerminal` passes
///   through unchanged: the owner-only relay path, where there is no central to
///   stamp and redeemed codes are hard-rejected at redeem time — identical to the
///   `RequestRemoteAccess` owner relay.
pub(super) fn gate_start_terminal_frame(
    model: SignalingModel,
    source: InboundSignalingSource,
    expected_audience: &str,
    now_rfc3339: &str,
) -> RequestRemoteGateOutcome {
    let has_wrapper = model
        .get_raw_data()
        .as_ref()
        .and_then(|v| v.as_object())
        .map(|o| o.contains_key("authz") && o.contains_key("inner"))
        .unwrap_or(false);

    if !has_wrapper {
        if source == InboundSignalingSource::TrustedCentral {
            return RequestRemoteGateOutcome::Drop(
                "bare StartTerminal from trusted-central source (capability-ceiling stamp required)"
                    .to_string(),
            );
        }
        return RequestRemoteGateOutcome::Pass(model, None);
    }

    if source != InboundSignalingSource::TrustedCentral {
        return RequestRemoteGateOutcome::Drop(format!(
            "StartTerminal carried a capability-ceiling stamp from non-central source {source:?}"
        ));
    }

    let raw = match model.get_raw_data().clone() {
        Some(v) => v,
        None => return RequestRemoteGateOutcome::Drop("stamped frame had no data".to_string()),
    };
    let wrapper: AuthorizedTerminalStart = match serde_json::from_value(raw) {
        Ok(w) => w,
        Err(e) => {
            return RequestRemoteGateOutcome::Drop(format!(
                "malformed StartTerminal stamp wrapper: {e}"
            ));
        }
    };

    if let Err(e) = wrapper
        .authz
        .validate(&model.request_id, expected_audience, now_rfc3339)
    {
        return RequestRemoteGateOutcome::Drop(format!("StartTerminal stamp rejected: {e:?}"));
    }

    let unwrapped = SignalingModel::new(
        &model.request_id,
        model.signaling_type,
        model.from_connection_id.clone(),
        model.to_connection_id.clone(),
        Some(wrapper.inner),
        model.response_state.clone(),
    );
    RequestRemoteGateOutcome::Pass(unwrapped, Some(wrapper.authz))
}
