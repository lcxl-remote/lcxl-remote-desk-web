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
/// - **Unknown `connection_id`** — a race against `CloseControl` /
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
    // the daemon's offer / canid handlers (which take the write lock)
    // are not blocked while the codec write completes.
    let (track_opt, paused) = {
        let g = ctx.read().await;
        let t = match frame.kind {
            MediaFrameKind::VideoI | MediaFrameKind::VideoP => g.video_track.clone(),
            MediaFrameKind::Audio => g.audio_track.clone(),
        };
        (t, g.media_paused.clone())
    };

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
                log::info!(
                    "[pc_manager] {} resumed media (first IDR after worker swap)",
                    frame.connection_id
                );
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
    if let Err(e) = track.write_sample(&sample).await {
        log::warn!(
            "[pc_manager] write_sample failed for {} ({:?}): {e}",
            frame.connection_id,
            frame.kind
        );
    }
}
