//! No Portal authorization or restore is reachable from this read operation.
use super::*;
pub(crate) mod service;
use crate::worker::agent::{computer_use_broker::ComputerUseBroker, linux_desktop};
use desk_agent_protocol::{ScreenFrameFreshness, ScreenFrameObservation};
use desk_capture_engine::{
    image_capture::wayland_portal_capture::WaylandPortalImageCapture,
    model::image_capture::ImageCapture,
};
use std::{
    sync::Arc,
    time::{Duration, Instant, UNIX_EPOCH},
};

#[derive(Clone)]
pub(crate) struct CachedFrame {
    pub(crate) output: ScreenCaptureOutput,
    pub(crate) received_at: Instant,
    pub(crate) continuity: desk_capture_engine::image_capture::pipewire_capture::CaptureContinuity,
    pub(crate) identity: String,
    pub(crate) input_epoch: u64,
    pub(crate) session: Arc<dyn desk_wayland_portal::LivePortalSession>,
}

pub(crate) async fn collect(
    broker: Arc<ComputerUseBroker>,
    params: ScreenCaptureParams,
    settings: DeskSettings,
) -> Result<ScreenCaptureOutput, AgentError> {
    let selection = params.clone();
    let settings =
        tokio::task::spawn_blocking(move || super::display::resolve(&settings, &selection))
            .await
            .map_err(|_| internal("display selection worker failed"))??;
    let worker_generation = broker.wayland_capture_generation();
    let identity = linux_desktop::resolve()
        .await
        .map_err(|_| internal("trusted GNOME Wayland session is unavailable"))?;
    if broker.linux_desktop_identity().as_ref() != Some(&identity) {
        return Err(internal("desktop identity changed"));
    }
    let portal = broker
        .portal()
        .ok_or_else(|| internal("Portal session is unavailable"))?;
    let session = portal
        .try_borrow_session(false)
        .map_err(|_| internal("authorize screen sharing locally before taking a screenshot"))?;
    let generation = portal.snapshot().await.generation;
    let input_epoch = broker.human_input_epoch();
    let closed = session.closure_token();
    let permit = broker.acquire_screen_capture_permit(&params, &settings.video_device_name)?;
    let captured = service::snapshot(
        &broker.wayland_snapshot_service,
        &broker,
        identity.binding(),
        generation,
        settings,
        session.clone(),
        permit,
    )
    .await
    .inspect_err(|_| {
        broker.clear_wayland_frame();
    })?;
    if closed.is_cancelled()
        || portal.snapshot().await.generation != generation
        || !portal
            .try_borrow_session(false)
            .is_ok_and(|current| Arc::ptr_eq(&current, &session))
        || linux_desktop::resolve().await.as_ref() != Ok(&identity)
        || broker.linux_desktop_identity().as_ref() != Some(&identity)
    {
        broker.clear_wayland_frame();
        return Err(internal("desktop or Portal session changed during capture"));
    }
    broker.publish_wayland_frame(
        &identity,
        worker_generation,
        CachedFrame {
            output: captured.output,
            received_at: captured.received_at,
            continuity: captured.continuity,
            identity: identity.binding(),
            input_epoch,
            session,
        },
    )
}

/// Count from native receipt, including encoding, queueing and session rechecks.
pub(crate) fn refresh_receipt_age(observation: &mut ScreenFrameObservation, received_at: Instant) {
    observation.receipt_age_ms = received_at
        .elapsed()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_age_includes_delayed_revalidation_without_upgrading_freshness() {
        let received_at = Instant::now().checked_sub(Duration::from_secs(8)).unwrap();
        let mut observation = ScreenFrameObservation {
            observation_id: "fixture".into(),
            output_reference: None,
            stream_generation: 7,
            received_at_unix_ms: 123,
            receipt_age_ms: 1,
            source_timestamp_ns: Some(456),
            freshness: ScreenFrameFreshness::LatestObserved,
        };
        refresh_receipt_age(&mut observation, received_at);
        assert!(observation.receipt_age_ms >= 8_000);
        assert_eq!(observation.received_at_unix_ms, 123);
        assert_eq!(observation.source_timestamp_ns, Some(456));
        assert_eq!(observation.stream_generation, 7);
        assert_eq!(observation.freshness, ScreenFrameFreshness::LatestObserved);
        let first = observation.receipt_age_ms;
        refresh_receipt_age(&mut observation, received_at);
        assert!(
            observation.receipt_age_ms >= first,
            "republication reset native age"
        );
    }
}
