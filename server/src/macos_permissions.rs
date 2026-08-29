//! macOS TCC (Transparency, Consent & Control) permission probing.
//!
//! Capabilities on macOS are gated by per-app, per-user TCC consent, orthogonal
//! to uid — root cannot bypass it. Screen capture, Accessibility automation,
//! and passive input observation use separate grants, so they are reported
//! independently rather than folded into `is_admin`.
//!
//! All ordinary probes are read-only and non-prompting:
//! `CGPreflightScreenCaptureAccess`, `AXIsProcessTrusted` (no options), and
//! `CGPreflightListenEventAccess` query their grants without showing prompts;
//! Apple Events uses `AEDeterminePermissionToAutomateTarget(..., false)`.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::model::info::MacosPermissions;

const NUMBERS_BUNDLE_ID: &str = "com.apple.Numbers";
const PAGES_BUNDLE_ID: &str = "com.apple.Pages";
const KEYNOTE_BUNDLE_ID: &str = "com.apple.Keynote";
const TYPE_APPLICATION_BUNDLE_ID: u32 = u32::from_be_bytes(*b"bund");
// Query the exact read-only Apple Event used by the iWork adapter. macOS can
// leave wildcard (`****`/`****`) Automation probes blocked indefinitely for an
// ad-hoc development identity, while a concrete core/getd decision returns or
// presents the normal consent flow.
const CORE_SUITE: u32 = u32::from_be_bytes(*b"core");
const GET_DATA_EVENT: u32 = u32::from_be_bytes(*b"getd");
const NO_ERR: i32 = 0;
const PROC_NOT_FOUND: i32 = -600;
const EVENT_NOT_PERMITTED: i32 = -1743;
const EVENT_WOULD_REQUIRE_CONSENT: i32 = -1744;
const AUTOMATION_PERMISSION_TIMEOUT: Duration = Duration::from_secs(1);

#[repr(C)]
struct AEDesc {
    descriptor_type: u32,
    data_handle: *mut c_void,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutomationPermissionState {
    Granted,
    Missing,
    TargetOffline,
    Failed,
}

struct AutomationPermissionRequest {
    ask_user_if_needed: bool,
    reply: Option<mpsc::Sender<AutomationPermissionState>>,
}

struct AutomationPermissionWorker {
    sender: SyncSender<AutomationPermissionRequest>,
    busy: Arc<AtomicBool>,
}

impl AutomationPermissionWorker {
    fn spawn(
        name: &str,
        operation: impl Fn(bool) -> AutomationPermissionState + Send + 'static,
    ) -> Option<Self> {
        let (sender, receiver) = mpsc::sync_channel::<AutomationPermissionRequest>(1);
        let busy = Arc::new(AtomicBool::new(false));
        let worker_busy = Arc::clone(&busy);
        thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    let result = operation(request.ask_user_if_needed);
                    worker_busy.store(false, Ordering::Release);
                    if let Some(reply) = request.reply {
                        let _ = reply.send(result);
                    }
                }
            })
            .ok()?;
        Some(Self { sender, busy })
    }

    fn begin(&self, ask_user_if_needed: bool) -> Option<Receiver<AutomationPermissionState>> {
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        let (reply, receiver) = mpsc::channel();
        let request = AutomationPermissionRequest {
            ask_user_if_needed,
            reply: Some(reply),
        };
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) =
            self.sender.try_send(request)
        {
            self.busy.store(false, Ordering::Release);
            return None;
        }
        Some(receiver)
    }

    fn query(&self, timeout: Duration) -> AutomationPermissionState {
        self.begin(false)
            .and_then(|receiver| receiver.recv_timeout(timeout).ok())
            .unwrap_or(AutomationPermissionState::Failed)
    }

    fn request(&self) {
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let request = AutomationPermissionRequest {
            ask_user_if_needed: true,
            reply: None,
        };
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) =
            self.sender.try_send(request)
        {
            self.busy.store(false, Ordering::Release);
        }
    }
}

// Screen Recording grant query. CoreGraphics is already linked by the
// `core-graphics` dependency; the explicit `#[link]` is harmless (frameworks
// dedupe) and documents the requirement.
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGPreflightListenEventAccess() -> bool;
}

// Accessibility (synthetic input / AX) grant query.
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AECreateDesc(
        type_code: u32,
        data: *const c_void,
        data_size: isize,
        result: *mut AEDesc,
    ) -> i32;
    fn AEDisposeDesc(descriptor: *mut AEDesc) -> i32;
    fn AEDeterminePermissionToAutomateTarget(
        target: *const AEDesc,
        event_class: u32,
        event_id: u32,
        ask_user_if_needed: u8,
    ) -> i32;
}

/// Probe the current process's screen-recording, Accessibility, and Input
/// Monitoring TCC grants.
pub fn probe() -> MacosPermissions {
    // SAFETY: both functions take no arguments, never block, and only read the
    // TCC database for the calling process.
    let screen_recording = unsafe { CGPreflightScreenCaptureAccess() };
    let accessibility = unsafe { AXIsProcessTrusted() };
    let input_monitoring = unsafe { CGPreflightListenEventAccess() };
    let deadline = Instant::now() + AUTOMATION_PERMISSION_TIMEOUT;
    let numbers_automation = begin_automation_permission(NUMBERS_BUNDLE_ID);
    let pages_automation = begin_automation_permission(PAGES_BUNDLE_ID);
    let keynote_automation = begin_automation_permission(KEYNOTE_BUNDLE_ID);
    MacosPermissions {
        screen_recording,
        accessibility,
        input_monitoring,
        numbers_automation: finish_automation_permission(numbers_automation, deadline)
            == AutomationPermissionState::Granted,
        pages_automation: finish_automation_permission(pages_automation, deadline)
            == AutomationPermissionState::Granted,
        keynote_automation: finish_automation_permission(keynote_automation, deadline)
            == AutomationPermissionState::Granted,
    }
}

/// Query Automation access for one compile-time-owned bundle identifier.
///
/// CoreServices may wait indefinitely while TCC validates a broken target code
/// signature. The dedicated per-target worker keeps that wait out of Actix and
/// bounds subsequent probes without creating an unbounded number of threads.
pub fn automation_permission(bundle_id: &'static str) -> AutomationPermissionState {
    permission_worker(bundle_id)
        .map(|worker| worker.query(AUTOMATION_PERMISSION_TIMEOUT))
        .unwrap_or(AutomationPermissionState::Failed)
}

fn begin_automation_permission(
    bundle_id: &'static str,
) -> Option<Receiver<AutomationPermissionState>> {
    permission_worker(bundle_id)?.begin(false)
}

fn finish_automation_permission(
    receiver: Option<Receiver<AutomationPermissionState>>,
    deadline: Instant,
) -> AutomationPermissionState {
    let Some(receiver) = receiver else {
        return AutomationPermissionState::Failed;
    };
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .unwrap_or(AutomationPermissionState::Failed)
}

fn request_automation_permission(bundle_id: &'static str) {
    if let Some(worker) = request_permission_worker(bundle_id) {
        worker.request();
    }
}

fn permission_worker(bundle_id: &'static str) -> Option<&'static AutomationPermissionWorker> {
    static NUMBERS: OnceLock<Option<AutomationPermissionWorker>> = OnceLock::new();
    static PAGES: OnceLock<Option<AutomationPermissionWorker>> = OnceLock::new();
    static KEYNOTE: OnceLock<Option<AutomationPermissionWorker>> = OnceLock::new();

    let slot = match bundle_id {
        NUMBERS_BUNDLE_ID => &NUMBERS,
        PAGES_BUNDLE_ID => &PAGES,
        KEYNOTE_BUNDLE_ID => &KEYNOTE,
        _ => return None,
    };
    slot.get_or_init(|| {
        AutomationPermissionWorker::spawn(
            &format!("macos-automation-{}", bundle_id.rsplit('.').next().unwrap()),
            move |ask_user_if_needed| automation_permission_raw(bundle_id, ask_user_if_needed),
        )
    })
    .as_ref()
}

/// Return the dedicated prompt lane for one Automation target.
///
/// CoreServices can leave a non-prompting permission query blocked forever
/// while validating a development build's code identity. Prompt requests must
/// not share that worker: otherwise the first status refresh permanently makes
/// the local "Request permissions" action a no-op.
fn request_permission_worker(
    bundle_id: &'static str,
) -> Option<&'static AutomationPermissionWorker> {
    static NUMBERS: OnceLock<Option<AutomationPermissionWorker>> = OnceLock::new();
    static PAGES: OnceLock<Option<AutomationPermissionWorker>> = OnceLock::new();
    static KEYNOTE: OnceLock<Option<AutomationPermissionWorker>> = OnceLock::new();

    let slot = match bundle_id {
        NUMBERS_BUNDLE_ID => &NUMBERS,
        PAGES_BUNDLE_ID => &PAGES,
        KEYNOTE_BUNDLE_ID => &KEYNOTE,
        _ => return None,
    };
    slot.get_or_init(|| {
        AutomationPermissionWorker::spawn(
            &format!(
                "macos-automation-prompt-{}",
                bundle_id.rsplit('.').next().unwrap()
            ),
            move |ask_user_if_needed| automation_permission_raw(bundle_id, ask_user_if_needed),
        )
    })
    .as_ref()
}

fn automation_permission_raw(
    bundle_id: &'static str,
    ask_user_if_needed: bool,
) -> AutomationPermissionState {
    let mut target = AEDesc {
        descriptor_type: 0,
        data_handle: std::ptr::null_mut(),
    };
    // SAFETY: CoreServices copies the bounded static bundle-id bytes and the
    // descriptor is disposed before this function returns.
    let created = unsafe {
        AECreateDesc(
            TYPE_APPLICATION_BUNDLE_ID,
            bundle_id.as_ptr().cast(),
            bundle_id.len() as isize,
            &mut target,
        )
    };
    if created != NO_ERR {
        return AutomationPermissionState::Failed;
    }
    // SAFETY: `target` remains valid for this synchronous TCC query. The call
    // runs only on a bounded, dedicated worker because the OS may not return.
    let status = unsafe {
        AEDeterminePermissionToAutomateTarget(
            &target,
            CORE_SUITE,
            GET_DATA_EVENT,
            u8::from(ask_user_if_needed),
        )
    };
    // SAFETY: ownership was returned by AECreateDesc above.
    let _ = unsafe { AEDisposeDesc(&mut target) };
    match status {
        NO_ERR => AutomationPermissionState::Granted,
        EVENT_NOT_PERMITTED | EVENT_WOULD_REQUIRE_CONSENT => AutomationPermissionState::Missing,
        PROC_NOT_FOUND => AutomationPermissionState::TargetOffline,
        _ => AutomationPermissionState::Failed,
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGRequestListenEventAccess() -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    static kAXTrustedCheckOptionPrompt: *const std::ffi::c_void;
    fn AXIsProcessTrustedWithOptions(options: *const std::ffi::c_void) -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: *const std::ffi::c_void;
    fn CFDictionaryCreate(
        allocator: *const std::ffi::c_void,
        keys: *const *const std::ffi::c_void,
        values: *const *const std::ffi::c_void,
        count: isize,
        key_callbacks: *const std::ffi::c_void,
        value_callbacks: *const std::ffi::c_void,
    ) -> *const std::ffi::c_void;
    fn CFRelease(value: *const std::ffi::c_void);
}

/// Ask macOS to present its Screen Recording, Accessibility, Input Monitoring,
/// and currently reachable iWork Automation consent flows. The return values
/// are deliberately ignored:
/// `/api/server_info` re-runs the non-prompting preflight and remains the only
/// readiness truth.
pub fn request() {
    // SAFETY: the CoreGraphics call has no arguments and only asks TCC to show
    // its standard consent UI for this process.
    let _ = unsafe { CGRequestScreenCaptureAccess() };
    let _ = unsafe { CGRequestListenEventAccess() };

    // The dictionary contains two immortal framework constants. Null callback
    // tables are intentional: the temporary dictionary neither owns nor
    // releases them, and pointer identity is the contract for this AX option.
    let keys = [unsafe { kAXTrustedCheckOptionPrompt }];
    let values = [unsafe { kCFBooleanTrue }];
    // SAFETY: both arrays remain alive for the call and contain one valid
    // CoreFoundation object pointer each.
    let options = unsafe {
        CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if !options.is_null() {
        // SAFETY: `options` is a +1 object returned by CFDictionaryCreate and
        // remains valid until the matching release below.
        let _ = unsafe { AXIsProcessTrustedWithOptions(options) };
        unsafe { CFRelease(options) };
    }

    // This function is reachable only through the loopback + same-origin local
    // onboarding endpoint. Each prompt runs on its target's bounded worker so
    // a stalled TCC dialog cannot occupy an HTTP runtime thread.
    for bundle_id in [NUMBERS_BUNDLE_ID, PAGES_BUNDLE_ID, KEYNOTE_BUNDLE_ID] {
        request_automation_permission(bundle_id);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{AutomationPermissionState, AutomationPermissionWorker};

    #[test]
    fn stalled_automation_query_is_bounded_and_coalesced() {
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = Arc::clone(&calls);
        let worker = AutomationPermissionWorker::spawn("test-automation-timeout", move |_| {
            worker_calls.fetch_add(1, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(200));
            AutomationPermissionState::Granted
        })
        .unwrap();

        let started = Instant::now();
        assert_eq!(
            worker.query(Duration::from_millis(30)),
            AutomationPermissionState::Failed
        );
        assert!(started.elapsed() < Duration::from_millis(150));

        let retry_started = Instant::now();
        assert_eq!(
            worker.query(Duration::from_millis(30)),
            AutomationPermissionState::Failed
        );
        assert!(retry_started.elapsed() < Duration::from_millis(30));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        thread::sleep(Duration::from_millis(220));
        assert_eq!(
            worker.query(Duration::from_millis(250)),
            AutomationPermissionState::Granted
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn stalled_query_lane_does_not_block_the_prompt_lane() {
        let query_worker = AutomationPermissionWorker::spawn("test-query-lane", move |_| {
            thread::sleep(Duration::from_millis(200));
            AutomationPermissionState::Granted
        })
        .unwrap();
        let prompt_calls = Arc::new(AtomicUsize::new(0));
        let worker_prompt_calls = Arc::clone(&prompt_calls);
        let prompt_worker =
            AutomationPermissionWorker::spawn("test-prompt-lane", move |ask_user_if_needed| {
                assert!(ask_user_if_needed);
                worker_prompt_calls.fetch_add(1, Ordering::SeqCst);
                AutomationPermissionState::Granted
            })
            .unwrap();

        assert_eq!(
            query_worker.query(Duration::from_millis(20)),
            AutomationPermissionState::Failed
        );
        prompt_worker.request();

        let deadline = Instant::now() + Duration::from_millis(100);
        while prompt_calls.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(prompt_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    #[ignore = "requires all three running iWork apps with Automation approval"]
    fn live_probe_reports_per_app_iwork_tcc_grants() {
        let permissions = super::probe();
        assert!(
            permissions.numbers_automation,
            "Numbers is offline or Automation permission is not granted to this test binary"
        );
        assert!(
            permissions.pages_automation,
            "Pages is offline or Automation permission is not granted to this test binary"
        );
        assert!(
            permissions.keynote_automation,
            "Keynote is offline or Automation permission is not granted to this test binary"
        );
    }
}
