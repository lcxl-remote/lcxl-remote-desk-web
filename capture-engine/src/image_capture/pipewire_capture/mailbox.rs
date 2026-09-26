//! One latest frame plus bounded metadata; a slow consumer cannot queue images.
use super::*;
use std::sync::{
    Arc, Condvar, Mutex,
    mpsc::{RecvTimeoutError, SendError},
};
use std::time::{Duration, Instant};

#[derive(Default)]
struct State {
    frame: Option<PipewireImageInfo>,
    discontinuity: bool,
    format: Option<VideoInfoRaw>,
    output: Option<DisplayInfo>,
    failure: Option<String>,
    senders: usize,
    receiver: bool,
    revision: u64,
}
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}
pub struct Sender(Arc<Shared>);
pub struct Receiver(Arc<Shared>);

/// A frame stays observable only while its producer and negotiated format survive.
/// This does not prove that the pixels still describe the current desktop.
#[derive(Clone)]
pub struct CaptureContinuity {
    shared: std::sync::Weak<Shared>,
    revision: u64,
}
impl CaptureContinuity {
    pub fn is_current(&self) -> bool {
        self.shared.upgrade().is_some_and(|shared| {
            shared.state.lock().is_ok_and(|state| {
                state.receiver
                    && state.senders > 0
                    && state.failure.is_none()
                    && state.revision == self.revision
                    && state.revision != u64::MAX
            })
        })
    }
}

pub(super) fn channel() -> (Sender, Receiver) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            senders: 1,
            receiver: true,
            ..State::default()
        }),
        changed: Condvar::new(),
    });
    (Sender(shared.clone()), Receiver(shared))
}

impl Clone for Sender {
    fn clone(&self) -> Self {
        self.0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .senders += 1;
        Self(self.0.clone())
    }
}
impl Drop for Sender {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .senders -= 1;
        self.0.changed.notify_all();
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.receiver = false;
        state.frame = None;
    }
}
impl Sender {
    pub fn send(&self, value: PipewireCallback) -> Result<(), SendError<PipewireCallback>> {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.receiver || state.failure.is_some() {
            return Err(SendError(value));
        }
        if !matches!(&value, PipewireCallback::ImageInfo(_)) {
            state.revision = state.revision.saturating_add(1);
        }
        match value {
            PipewireCallback::ImageInfo(frame) => state.frame = Some(frame),
            PipewireCallback::Discontinuity => {
                state.frame = None;
                state.discontinuity = true;
            }
            PipewireCallback::Format(format) => {
                state.frame = None;
                state.format = Some(format);
            }
            PipewireCallback::CurrentOutput(output) => {
                state.frame = None;
                state.output = Some(output);
            }
            PipewireCallback::Failure(reason) => {
                state.frame = None;
                state.failure = Some(reason);
            }
        }
        self.0.changed.notify_one();
        Ok(())
    }
}

fn take(state: &mut State) -> Option<PipewireCallback> {
    if let Some(reason) = &state.failure {
        return Some(PipewireCallback::Failure(reason.clone()));
    }
    if std::mem::take(&mut state.discontinuity) {
        return Some(PipewireCallback::Discontinuity);
    }
    if let Some(output) = state.output.take() {
        return Some(PipewireCallback::CurrentOutput(output));
    }
    if let Some(format) = state.format.take() {
        return Some(PipewireCallback::Format(format));
    }
    state.frame.take().map(PipewireCallback::ImageInfo)
}
impl Receiver {
    pub fn try_iter(&self) -> std::vec::IntoIter<PipewireCallback> {
        self.take_observation().0
    }
    pub(super) fn take_observation(
        &self,
    ) -> (std::vec::IntoIter<PipewireCallback>, CaptureContinuity) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut values = Vec::with_capacity(4);
        while let Some(value) = take(&mut state) {
            let failure = matches!(value, PipewireCallback::Failure(_));
            values.push(value);
            if failure {
                break;
            }
        }
        if values.is_empty() && state.senders == 0 {
            values.push(PipewireCallback::Failure(
                "PipeWire callback producer stopped".into(),
            ));
        }
        let continuity = CaptureContinuity {
            shared: Arc::downgrade(&self.0),
            revision: state.revision,
        };
        (values.into_iter(), continuity)
    }
    pub fn recv_timeout(&self, timeout: Duration) -> Result<PipewireCallback, RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(value) = take(&mut state) {
                return Ok(value);
            }
            if state.senders == 0 {
                return Err(RecvTimeoutError::Disconnected);
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(RecvTimeoutError::Timeout);
            };
            state = self
                .0
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(value: u8) -> PipewireCallback {
        PipewireCallback::ImageInfo(PipewireImageInfo {
            source_timestamp_ns: None,
            received_at: (std::time::SystemTime::now(), Instant::now()),
            image_type: ImageType::BGRA,
            data: vec![value; 4],
            width: 1,
            height: 1,
        })
    }
    #[test]
    fn discontinuity_revokes_cached_image_but_allows_a_new_observation() {
        let (sender, receiver) = channel();
        sender.send(frame(1)).unwrap();
        let (_, old) = receiver.take_observation();
        assert!(old.is_current());
        sender.send(PipewireCallback::Discontinuity).unwrap();
        assert!(!old.is_current());
        let (items, _) = receiver.take_observation();
        let values: Vec<_> = items.collect();
        assert_eq!(values.len(), 1);
        assert!(matches!(values[0], PipewireCallback::Discontinuity));
        sender.send(frame(2)).unwrap();
        let (mut items, current) = receiver.take_observation();
        assert!(matches!(items.next(), Some(PipewireCallback::ImageInfo(_))));
        assert!(current.is_current());
        assert!(!old.is_current());
    }

    #[test]
    fn source_presentation_time_follows_the_latest_image() {
        let (sender, receiver) = channel();
        let PipewireCallback::ImageInfo(mut first) = frame(1) else {
            unreachable!()
        };
        first.source_timestamp_ns = Some(123);
        sender.send(PipewireCallback::ImageInfo(first)).unwrap();
        let PipewireCallback::ImageInfo(mut second) = frame(2) else {
            unreachable!()
        };
        second.source_timestamp_ns = Some(456);
        sender.send(PipewireCallback::ImageInfo(second)).unwrap();
        let (mut values, _) = receiver.take_observation();
        let Some(PipewireCallback::ImageInfo(image)) = values.next() else {
            panic!("latest frame missing")
        };
        assert_eq!(image.source_timestamp_ns(), Some(456));
        assert_eq!(image.data, vec![2; 4]);
    }

    #[test]
    fn continuity_tracks_format_and_lifetime_without_claiming_pixel_freshness() {
        let (sender, receiver) = channel();
        sender.send(frame(1)).unwrap();
        let (_, first) = receiver.take_observation();
        assert!(first.is_current());
        sender.send(frame(2)).unwrap();
        // New pixels do not invalidate continuity, which is not a freshness fence.
        assert!(first.is_current());
        sender
            .send(PipewireCallback::Format(VideoInfoRaw::default()))
            .unwrap();
        assert!(!first.is_current());
        let (_, next) = receiver.take_observation();
        assert!(next.is_current());
        drop(sender);
        assert!(!next.is_current());
    }

    #[test]
    fn continuity_does_not_keep_closed_capture_alive() {
        let (sender, receiver) = channel();
        let (_, token) = receiver.take_observation();
        drop(receiver);
        assert!(!token.is_current());
        drop(sender);
        assert!(token.shared.upgrade().is_none());
        let (sender, receiver) = channel();
        let (_, token) = receiver.take_observation();
        sender
            .send(PipewireCallback::Failure("closed".into()))
            .unwrap();
        assert!(!token.is_current());
    }

    #[test]
    fn format_change_discards_old_pixels_even_without_a_replacement_frame() {
        let (sender, receiver) = channel();
        sender.send(frame(1)).unwrap();
        sender
            .send(PipewireCallback::Format(VideoInfoRaw::default()))
            .unwrap();
        let values = receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(values.len(), 1);
        assert!(matches!(&values[0], PipewireCallback::Format(_)));
        assert!(receiver.try_iter().next().is_none());
        sender.send(frame(2)).unwrap();
        let values = receiver.try_iter().collect::<Vec<_>>();
        assert!(matches!(&values[0], PipewireCallback::ImageInfo(image) if image.data == [2; 4]));
    }

    #[test]
    fn slow_consumer_receives_only_latest_frame_and_failure_discards_it() {
        let (sender, receiver) = channel();
        for n in 0..100 {
            sender.send(frame(n)).unwrap();
        }
        let values = receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(values.len(), 1);
        assert!(matches!(&values[0], PipewireCallback::ImageInfo(image) if image.data == [99; 4]));
        sender.send(frame(1)).unwrap();
        sender
            .send(PipewireCallback::Failure("stream failed".into()))
            .unwrap();
        assert!(matches!(
            receiver.try_iter().next(),
            Some(PipewireCallback::Failure(_))
        ));
        assert!(sender.send(frame(2)).is_err());
        assert!(matches!(
            receiver.try_iter().next(),
            Some(PipewireCallback::Failure(_))
        ));
    }
}
