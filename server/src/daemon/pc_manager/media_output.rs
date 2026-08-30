//! Worker media-frame delivery to active WebRTC tracks.

use super::*;

// =====================================================================
// MediaFrame ingestion
// =====================================================================

/// Write one decoded `MediaFrame` to the appropriate per-`connection_id`
/// `TrackLocalStaticSample`. Called from the daemon-side media-pipe
/// receiver task spawned by `worker_manager::run_pipe_server`.
///
/// All errors are intentionally swallowed:
///
/// - **Unknown `connection_id`** — a race against `CloseRemoteSession` /
///   browser drop. Logged at trace level so high-rate noise during
///   normal teardown does not flood the operator.
/// - **No `video_track` yet (Audio frame, or video before the first
///   `Offer` arrived)** — same race window; debug-logged and skipped.
/// - **`write_sample` failure** — surfaced as a warning. The sample is
///   dropped; the next IDR will resync. We do not propagate the error
///   because the caller is a long-running receiver loop and there is
///   nothing useful to do at that level besides keep reading frames.
///
/// Video and audio frames are shaped through the same entry point,
/// differing only in which per-connection track they target.
pub async fn write_video_frame(registry: &PcRegistry, frame: MediaFrame) {
    let ctx = match registry.get(&frame.connection_id).await {
        Some(c) => c,
        None => {
            log::trace!(
                "[pc_manager] dropping frame for unknown connection {}",
                frame.connection_id
            );
            return;
        }
    };

    // Hold the read guard only as long as we need the track Arc + the
    // pause flag; clone them out before awaiting on `write_sample` so
    // the daemon's offer / ice_candidate handlers (which take the write lock)
    // are not blocked while the codec write completes.
    let (track_opt, paused, fence, switch_timing) = {
        let g = ctx.read().await;
        let t = match frame.kind {
            MediaFrameKind::VideoI | MediaFrameKind::VideoP => g.video_track.clone(),
            MediaFrameKind::Audio => g.audio_track.clone(),
        };
        (
            t,
            g.media_paused.clone(),
            Arc::clone(&g.media_output_fence),
            Arc::clone(&g.media_switch_timing),
        )
    };

    // Keep the read guard through `write_sample`: revocation takes the write
    // side, so once it returns no older audio write can still be in flight.
    let fence_guard = fence.read().await;
    let generation_matches = match frame.kind {
        MediaFrameKind::VideoI | MediaFrameKind::VideoP => {
            fence_guard.video_epoch == frame.connection_epoch
                && fence_guard.video_generation == frame.generation
        }
        MediaFrameKind::Audio => {
            fence_guard.audio_open
                && fence_guard.audio_epoch == frame.connection_epoch
                && fence_guard.audio_generation == frame.generation
        }
    };
    if !generation_matches {
        log::trace!(
            "[pc_manager] dropping stale/closed {:?} frame for {} epoch={} generation={}",
            frame.kind,
            frame.connection_id,
            frame.connection_epoch,
            frame.generation
        );
        return;
    }

    // While a worker swap is in progress every frame except the
    // first IDR is dropped. Writing P frames or audio against the
    // browser's existing reference would either decode wrong (P) or
    // play sound against a frozen video frame (audio). The first
    // VideoI clears the flag in place — single store per swap, no
    // central coordinator needed because the same task that observes
    // `paused == true` is the one that flips it back. The flag-flip
    // happens BEFORE the track-presence check so the resume contract
    // (an IDR always re-arms the PC) holds even in the unusual case
    // where the offer hasn't reinstalled the track yet.
    if paused.load(Ordering::Relaxed) {
        match frame.kind {
            MediaFrameKind::VideoI => {
                paused.store(false, Ordering::Relaxed);
                if let Some(timing) = switch_timing.lock().unwrap().take() {
                    log::info!(
                        "resident_switch stage=first_idr connection={} route_epoch={} elapsed_ms={} connection_epoch={} generation={}",
                        frame.connection_id,
                        timing
                            .route_epoch
                            .map_or_else(|| "legacy".to_string(), |epoch| epoch.to_string()),
                        timing.started_at.elapsed().as_millis(),
                        frame.connection_epoch,
                        frame.generation
                    );
                }
                // fall through to write_sample
            }
            MediaFrameKind::VideoP | MediaFrameKind::Audio => {
                log::trace!(
                    "[pc_manager] dropping {:?} for {} during worker swap (waiting for IDR)",
                    frame.kind,
                    frame.connection_id
                );
                return;
            }
        }
    }

    let track = match track_opt {
        Some(t) => t,
        None => {
            log::debug!(
                "[pc_manager] dropping {:?} frame for {} — no matching track on PC yet \
                 (offer not exchanged?)",
                frame.kind,
                frame.connection_id
            );
            return;
        }
    };

    let sample = Sample {
        data: bytes::Bytes::from(frame.payload),
        duration: Duration::from_nanos(frame.duration_ns),
        ..Default::default()
    };
    // Both media kinds share the output fence. An unbounded video write would
    // therefore also prevent a security-driven audio revocation from taking
    // the write side of that fence. Bound every WebRTC write uniformly.
    let write_result = tokio::time::timeout(Duration::from_secs(1), track.write_sample(&sample))
        .await
        .map_err(|_| format!("{:?} write timed out", frame.kind))
        .and_then(|result| result.map_err(|e| e.to_string()));
    if let Err(e) = write_result {
        log::warn!(
            "[pc_manager] write_sample failed for {} ({:?}): {e}",
            frame.connection_id,
            frame.kind
        );
    }
}
