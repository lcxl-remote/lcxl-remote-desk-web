use super::*;

/// Decide which `HostControlHub` flavour to construct from an Init payload.
/// Returns the hub and, when running in Forwarder mode, the spec needed for the
/// caller to spawn the ws-client task. Split out from `ipc_loop` so the
/// decision can be unit-tested without an actix runtime.
pub(super) fn build_hub_from_init(
    payload: &WorkerInitPayload,
) -> (
    Arc<HostControlHub>,
    Option<(Arc<UpstreamForwarder>, String, String)>,
) {
    match payload.host_upstream_url.clone() {
        Some(url) => {
            let upstream = UpstreamForwarder::new();
            let token = payload.auth_token.clone().unwrap_or_default();
            let hub = Arc::new(HostControlHub::new_forwarder(Arc::clone(&upstream)));
            (hub, Some((upstream, url, token)))
        }
        None => (Arc::new(HostControlHub::new_local()), None),
    }
}

/// Convert a typed `ServiceToWorker` payload into the `SignalingModel`
/// consumed by the shared `DeskSession::handle_message` dispatcher.
///
/// Build / serialise failures are non-fatal: they log + drop, same
/// behaviour the previous `SignalingMessage` JSON-bridge path had on
/// malformed input.
pub(super) async fn dispatch_typed_signaling<T>(
    desk_session: &mut DeskSession,
    signaling_type: SignalingType,
    connection_id: Option<String>,
    data: &T,
) where
    T: serde::Serialize + ?Sized,
{
    dispatch_typed_signaling_with_request_id(
        desk_session,
        signaling_type,
        // One-way notifications carry no request_id; a placeholder
        // keeps logs scannable.
        "typed-ipc".to_string(),
        connection_id,
        Some(data),
    )
    .await
}

/// Same as [`dispatch_typed_signaling`] but lets the caller pass an
/// explicit `request_id` (manager-plane requests need to echo it
/// back so the worker's `send_response` writes the same id, which
/// the desk_rx outbound classifier then re-uses on the typed
/// `Manager*Response`).
///
/// Also accepts an `Option<&T>` body so empty-body requests
/// (`GetSystemInfo`) can
/// share this helper without serialising a synthetic placeholder.
///
/// `connection_id` is `Option<String>` because retained non-file manager
/// requests and `ListTerminalCommands` can originate from an HTTP controller without
/// a browser PC. Interactive file list and delete requests always provide a
/// trusted controller connection and fail closed before reaching this helper.
pub(super) async fn dispatch_typed_signaling_with_request_id<T>(
    desk_session: &mut DeskSession,
    signaling_type: SignalingType,
    request_id: String,
    connection_id: Option<String>,
    data: Option<&T>,
) where
    T: serde::Serialize + ?Sized,
{
    let signaling_data = match data {
        Some(d) => match serde_json::to_value(d) {
            Ok(v) => Some(v),
            Err(e) => {
                warn!(
                    "Failed to serialise {signaling_type:?} payload for {connection_id:?}: \
                     {e}; dropping",
                );
                return;
            }
        },
        None => None,
    };
    let model = SignalingModel::new(
        &request_id,
        signaling_type,
        connection_id.clone(),
        None,
        signaling_data,
        None,
    );
    if let Err(e) = desk_session.handle_message(&model).await {
        warn!(
            "DeskSession handle_message error for typed {signaling_type:?}: {e}, \
             connection_id={connection_id:?}, request_id={request_id}",
        );
        let response_type = match signaling_type {
            SignalingType::SetPrivateScreenVisibility => {
                Some(SignalingType::PrivateScreenVisibilitySet)
            }
            SignalingType::GetSystemInfo => Some(SignalingType::SystemInfoRetrieved),
            SignalingType::ListFiles => Some(SignalingType::FilesListed),
            SignalingType::DeleteFile => Some(SignalingType::FileDeleted),
            SignalingType::StartTerminal => Some(SignalingType::TerminalStarted),
            SignalingType::ListTerminalCommands => Some(SignalingType::TerminalCommandsListed),
            _ => None,
        };
        if let Some(response_type) = response_type {
            let _ = desk_session
                .session
                .send_error(
                    &request_id,
                    response_type,
                    connection_id,
                    e.to_error_code(),
                    &e.to_string(),
                )
                .await;
        }
    }
}

/// Classify an outbound signaling text blob produced by `DeskSession`
/// into a typed `WorkerToService` variant. Handles three groups:
///
/// 1. **Error responses** (`response_state.error_code != 0`): packed
///    into [`WorkerToService::SignalingError`] regardless of the
///    originating `SignalingType`. Catches every
///    `service::signaling::DeskSession::send_error` call (terminal
///    permission denied, manager file errors, the fallthrough
///    `_ => UNKNOWN_SIGNALING_TYPE`, ...).
/// 2. **Typed success responses / notifications** for migrated
///    SignalingTypes (PrivateScreenStateChanged, Manager*, Terminal*):
///    routed via [`try_route_typed_outbound`].
/// 3. **Anything else**: log + drop. Returns `None`. There is no
///    `SignalingMessage` fallback bridge — every outbound type the daemon needs to
///    surface to the browser is explicitly typed. A `None` result
///    indicates either a parse failure (malformed JSON the worker
///    never produced under normal operation) or a `SignalingType` no
///    longer expected on the worker → daemon path.
pub(super) fn build_outbound_payload_from_desk_text(text: String) -> Option<WorkerToService> {
    let model = match serde_json::from_str::<SignalingModel>(&text) {
        Ok(m) => m,
        Err(e) => {
            warn!(
                "Worker emitted unparseable signaling JSON; dropping (len={}, err={e})",
                text.len()
            );
            return None;
        }
    };

    // Error responses: route ALL of them through the typed
    // SignalingError catch-all — the original SignalingType is
    // preserved in the payload so the daemon can rebuild the
    // outbound `SignalingModel::error(...)` and the browser keys
    // off `signaling_type` to match the response to its pending
    // request.
    if let Some(state) = model.response_state.as_ref()
        && !state.is_success()
    {
        let connection_id = model.to_connection_id.clone().unwrap_or_default();
        return Some(WorkerToService::SignalingError(SignalingErrorPayload {
            request_id: model.request_id.clone(),
            connection_id,
            signaling_type: model.signaling_type,
            error_code: state.error_code,
            error_message: state.message.clone(),
        }));
    }

    if let Some(typed) = try_route_typed_outbound(&model) {
        return Some(typed);
    }

    warn!(
        "Worker emitted signaling reply with no typed IPC route: type={:?}, request_id={}; \
         dropping (the SignalingMessage bridge no longer exists — every outbound type must \
         have a typed variant)",
        model.signaling_type, model.request_id,
    );
    None
}

/// Return `Some(typed)` when the outbound `SignalingModel` matches a
/// success-response or notification type that has been promoted to a
/// typed `WorkerToService` variant; `None` for unmatched types (the
/// caller logs + drops). Error responses are handled separately by
/// the SignalingError branch in `build_outbound_payload_from_desk_text`.
/// Splits per-type matching out so each batch can append arms without
/// the surrounding function getting unwieldy.
pub(super) fn try_route_typed_outbound(model: &SignalingModel) -> Option<WorkerToService> {
    match model.signaling_type {
        SignalingType::PrivateScreenStateChanged | SignalingType::PrivateScreenVisibilitySet => {
            let connection_id = model.to_connection_id.clone()?;
            let data = model
                .get_data_with_type::<PrivateScreenStateChangedData>()
                .ok()
                .flatten()?;
            Some(WorkerToService::PrivateScreenStateChanged(
                PrivateScreenStateChangedPayload {
                    request_id: if model.signaling_type == SignalingType::PrivateScreenVisibilitySet
                    {
                        Some(model.request_id.clone())
                    } else {
                        None
                    },
                    connection_id,
                    data,
                },
            ))
        }
        // Manager-plane responses. `send_response` echoes
        // `from_connection_id` of the inbound request as
        // `to_connection_id` of the outbound response; HTTP-API-
        // triggered manager requests carry `None`, so the typed
        // payload also carries `Option<String>` and the daemon
        // correlates the response by `request_id` alone.
        SignalingType::SystemInfoRetrieved => {
            let info = model.get_data_with_type::<SystemInfo>().ok().flatten()?;
            Some(WorkerToService::SystemInfoRetrieved(
                SystemInfoRetrievedPayload {
                    request_id: model.request_id.clone(),
                    connection_id: model.to_connection_id.clone(),
                    info,
                },
            ))
        }
        SignalingType::FilesListed => {
            let response = model
                .get_data_with_type::<FileListResponse>()
                .ok()
                .flatten()?;
            Some(WorkerToService::FilesListed(FilesListedPayload {
                request_id: model.request_id.clone(),
                connection_id: model.to_connection_id.clone(),
                response,
            }))
        }
        // The DeleteFile response carries an empty body (`&()`), so a
        // successful round-trip omits signaling_data; `request_id` alone is
        // enough to correlate back to either an originating internal request or
        // the browser PC named by `to_connection_id`.
        SignalingType::FileDeleted => {
            Some(WorkerToService::FileDeleted(ManagerResponseRefPayload {
                request_id: model.request_id.clone(),
                connection_id: model.to_connection_id.clone(),
            }))
        }
        // Terminal-plane responses and notifications. The
        // worker's terminal handlers always set the target browser in
        // `to_connection_id` (either via `success_response` for
        // `TerminalStarted` / `ListTerminalCommands`, or via `new_request` for
        // server-initiated `TerminalOutputProduced` / `TerminalClosed`).
        SignalingType::TerminalStarted => {
            let connection_id = model.to_connection_id.clone()?;
            Some(WorkerToService::TerminalStarted(TerminalStartedPayload {
                request_id: model.request_id.clone(),
                connection_id,
            }))
        }
        SignalingType::TerminalClosed => {
            let connection_id = model.to_connection_id.clone()?;
            Some(WorkerToService::TerminalClosed(TerminalClosedPayload {
                connection_id,
            }))
        }
        SignalingType::TerminalOutputProduced => {
            let connection_id = model.to_connection_id.clone()?;
            let data = model
                .get_data_with_type::<TerminalOutputData>()
                .ok()
                .flatten()?;
            Some(WorkerToService::TerminalOutputProduced(
                TerminalOutputProducedPayload {
                    connection_id,
                    data,
                },
            ))
        }
        SignalingType::TerminalCommandsListed => {
            let terminals = model.get_data_with_type::<TerminalList>().ok().flatten()?;
            Some(WorkerToService::TerminalCommandsListed(
                TerminalCommandsListedPayload {
                    request_id: model.request_id.clone(),
                    connection_id: model.to_connection_id.clone(),
                    terminals,
                },
            ))
        }
        _ => None,
    }
}

/// Returns `true` iff the worker should refresh the input dispatcher's
/// per-connection geometry after sending this `WorkerToService` response
/// to the daemon. Specifically: when the response is a
/// `VirtualDisplayMode` reply carrying `VirtualDisplayModeOutcome::Applied`
/// — i.e. the IDD driver actually committed a new mode. `Failed` /
/// non-VirtualDisplayMode variants return `false`.
///
/// **Scope note**: this gate now only governs the input geometry
/// refresh, NOT the WGC capture restart. The IDD driver's
/// `Departure`+`Arrival` cycle happens at the pipe layer *before* the
/// CDS commit runs, so a `DISP_CHANGE_BADMODE` Failed outcome still
/// invalidates WGC's HMONITOR. The restart path is therefore
/// decoupled from this predicate in the IPC handler.
///
/// Pulled out of the main IPC loop so it can be unit-tested without
/// running the full session.
pub(super) fn should_refresh_after_set_mode(response: &WorkerToService) -> bool {
    matches!(
        response,
        WorkerToService::VirtualDisplayMode(p)
            if matches!(
                p.outcome,
                desk_ipc_protocol::message::VirtualDisplayModeOutcome::Applied(_)
            )
    )
}

/// Filter the candidate restart steps down to the connections that
/// actually need a forced capture-pipeline rebuild after a
/// `SetVirtualDisplayMode` Applied: only those whose effective
/// `CaptureKey` is WGC backend targeting the currently attached IDD
/// display. DXGI (returns `DXGI_ERROR_ACCESS_LOST` on
/// `AcquireNextFrame`) and GDI (re-`EnumDisplaySettingsW` every frame)
/// self-adapt to a mid-session monitor remount — forcing a Stop+Start
/// on them would only add a needless IDR flicker.
///
/// `key_lookup` is parameterised so the test suite can drive arbitrary
/// connection → key fixtures without spinning up a real producer.
pub(super) fn select_wgc_restart_steps<F>(
    steps: Vec<RestartStep>,
    attached_display: Option<&str>,
    key_lookup: F,
) -> Vec<RestartStep>
where
    F: Fn(&str) -> Option<CaptureKey>,
{
    let Some(attached) = attached_display else {
        return Vec::new();
    };
    steps
        .into_iter()
        .filter(|s| {
            key_lookup(&s.connection_id)
                .is_some_and(|k| k.backend.eq_ignore_ascii_case("WGC") && k.device_name == attached)
        })
        .collect()
}

/// Distinct `CaptureKey`s covered by the given restart steps, in
/// first-seen order. Ensures each shared-capture registry slot is
/// invalidated exactly once even when several connections share the
/// same `(backend, device_name)` slot (the multi-browser case).
pub(super) fn dedup_capture_keys<F>(steps: &[RestartStep], key_lookup: F) -> Vec<CaptureKey>
where
    F: Fn(&str) -> Option<CaptureKey>,
{
    let mut seen: HashSet<CaptureKey> = HashSet::new();
    let mut out: Vec<CaptureKey> = Vec::new();
    for step in steps {
        if let Some(k) = key_lookup(&step.connection_id)
            && seen.insert(k.clone())
        {
            out.push(k);
        }
    }
    out
}

/// React to a display-change event by refreshing per-connection cursor
/// geometry. On Wayland this resolves each connection's captured surface
/// against the live `wl_output` geometry (no portal picker); on every
/// other platform it re-queries the attached-display list.
#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
pub(super) fn refresh_geometry_after_display_change(
    input_dispatcher: &InputDispatcher,
    media_producer: Option<&MediaProducer>,
) {
    #[cfg(target_os = "linux")]
    {
        use desk_input_injection::linux_display::{Backend, detect_backend};
        if detect_backend() == Backend::Wayland {
            refresh_wayland_geometry(input_dispatcher, media_producer);
            return;
        }
    }
    input_dispatcher.refresh_geometry(None);
}

/// Wayland display-change refresh. Enumerates the current `wl_output`
/// logical geometry without the portal, then re-anchors each connection
/// on the captured surface's recorded position. Anything unresolved
/// (no producer, empty/failed enumeration, no anchor match) leaves the
/// connection's geometry untouched — it never re-points to a different
/// monitor.
#[cfg(target_os = "linux")]
pub(super) fn refresh_wayland_geometry(
    input_dispatcher: &InputDispatcher,
    media_producer: Option<&MediaProducer>,
) {
    use desk_capture_engine::image_capture::wayland_output_geometry::{
        enumerate_wayland_outputs, match_output_by_anchor,
    };
    let Some(producer) = media_producer else {
        return;
    };
    let outputs = match enumerate_wayland_outputs() {
        Ok(o) if !o.is_empty() => o,
        Ok(_) => {
            log::debug!(
                "display-change: no Wayland output geometry available; keeping current geometry"
            );
            return;
        }
        Err(e) => {
            log::debug!(
                "display-change: Wayland output enumeration failed: {e}; keeping current geometry"
            );
            return;
        }
    };
    for id in input_dispatcher.connection_ids() {
        let Some(info) = producer.connection_display_info(&id) else {
            continue;
        };
        let anchor = info.desktop_coordinates;
        match match_output_by_anchor(&outputs, anchor) {
            Some(geo) => {
                let r = geo.logical;
                input_dispatcher
                    .set_connection_geometry(&id, (r.left, r.top, r.width(), r.height()));
            }
            None => log::debug!(
                "display-change: no Wayland output matches connection {id} anchor {anchor:?}; \
                 keeping current geometry"
            ),
        }
    }
}
