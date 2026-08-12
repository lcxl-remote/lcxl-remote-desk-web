//! Trusted-central wrapper and fleet-exec authorization gates.

use super::*;

/// True for the trusted-central plumbing frames that drive an evidence
/// collection or a remote read-tool call. Unlike the AI control frames these may
/// arrive either bare or wrapped, so they get the optional-wrapper gate rather
/// than [`gate_authz_frame`]'s require-wrapper rule.
pub(super) fn is_central_plumbing_frame(t: SignalingType) -> bool {
    matches!(
        t,
        SignalingType::CollectEvidence | SignalingType::InvokeRemoteTool
    )
}

/// Optional-wrapper gate for trusted-central plumbing (`CollectRequest` /
/// `RemoteToolRequest`). The caller has already confirmed the trusted-central
/// source. These frames may arrive either:
///
/// - **bare** — the legacy / enterprise-manager path emits the raw payload; the
///   trusted-central link authentication is the trust anchor, so a bare frame
///   passes through to the router unchanged; or
/// - **wrapped** in an [`AuthorizedControlPayload`] — an OSS signal central brain
///   stamps and wraps every frame. A wrapper is validated against the frame's
///   `request_id`, this daemon's audience, and expiry (replay / misroute
///   defense-in-depth), then unwrapped to its inner payload for the router.
///
/// A wrapper that fails validation is dropped (no denied-result protocol exists
/// for these read-only frames; the central reaper times the pending entry out).
pub(super) fn gate_optional_central_wrapper(
    model: SignalingModel,
    expected_audience: &str,
    now_rfc3339: &str,
) -> AuthzGateOutcome {
    let has_wrapper = model
        .get_raw_data()
        .as_ref()
        .and_then(|v| v.as_object())
        .map(|o| o.contains_key("authz") && o.contains_key("inner"))
        .unwrap_or(false);

    if !has_wrapper {
        return AuthzGateOutcome::Pass(model, None);
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

/// Outcome of the dedicated `EdgeExecRequest` authorization gate. Unlike the
/// generic [`gate_authz_frame`] (which drops a frame whose wrapper fails to
/// validate), a fleet request from the trusted-central link that fails
/// validation is answered with a synthesized denied result so the central
/// pending entry resolves rather than hanging. Only a frame that cannot be
/// correlated at all (no `request_id`) is dropped outright.
#[derive(Debug)]
pub(super) enum FleetExecGateOutcome {
    /// Validated: the unwrapped frame (data = inner `ExecPlan`) plus the
    /// validated authorization block to thread into the router handler.
    Pass(SignalingModel, AuthorizationBlock),
    /// Trusted source but the request is unauthorized / malformed; answer the
    /// central brain with a `RejectedBeforeDispatch` carrying `reason`.
    Denied { request_id: String, reason: String },
    /// Uncorrelatable garbage; drop silently (no result can be attributed).
    Drop(String),
}

/// Dedicated authorization gate for `EdgeExecRequest` (central → daemon). The
/// caller has already confirmed the trusted-central source. Validates the
/// `AuthorizedControlPayload<ExecPlan>` wrapper; on success unwraps the inner
/// plan and returns the validated authorization block.
pub(super) fn gate_fleet_exec_frame(
    model: SignalingModel,
    expected_audience: &str,
    now_rfc3339: &str,
) -> FleetExecGateOutcome {
    let request_id = model.request_id.clone();
    if request_id.is_empty() {
        return FleetExecGateOutcome::Drop(
            "EdgeExecRequest without request_id (cannot correlate a result)".to_string(),
        );
    }

    let raw = match model.get_raw_data().clone() {
        Some(v) => v,
        None => {
            return FleetExecGateOutcome::Denied {
                request_id,
                reason: "pep_rejected:authz:missing_payload".to_string(),
            };
        }
    };
    let wrapper: AuthorizedControlPayload<serde_json::Value> = match serde_json::from_value(raw) {
        Ok(w) => w,
        Err(e) => {
            return FleetExecGateOutcome::Denied {
                request_id,
                reason: format!("pep_rejected:authz:malformed_wrapper:{e}"),
            };
        }
    };

    if let Err(e) = wrapper
        .authz
        .validate(&request_id, expected_audience, now_rfc3339)
    {
        return FleetExecGateOutcome::Denied {
            request_id,
            reason: format!("pep_rejected:authz:{e:?}"),
        };
    }

    let unwrapped = SignalingModel::new(
        &request_id,
        model.signaling_type,
        model.from_connection_id.clone(),
        model.to_connection_id.clone(),
        Some(wrapper.inner),
        model.response_state.clone(),
    );
    FleetExecGateOutcome::Pass(unwrapped, wrapper.authz)
}
