//! Interactive tests create and destroy only their own native windows.
use super::*;
use std::sync::mpsc;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

struct Fixture {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Fixture {
    fn start() -> Self {
        Self::with_password(false)
    }

    fn with_password(password: bool) -> Self {
        Self::with_controls(password, false)
    }

    fn with_controls(password: bool, scroll: bool) -> Self {
        let (stop, stopped) = mpsc::channel();
        let (ready, started) = mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            let mut handles = Vec::new();
            for (index, title) in [w!("Parity fixture alpha"), w!("Parity fixture beta")]
                .into_iter()
                .enumerate()
            {
                let hwnd = unsafe {
                    CreateWindowExW(
                        WS_EX_NOACTIVATE,
                        w!("STATIC"),
                        title,
                        WS_OVERLAPPEDWINDOW,
                        40 + index as i32 * 40,
                        40,
                        240 + index as i32 * 40,
                        160,
                        None,
                        None,
                        None,
                        None,
                    )
                }
                .expect("create fixture window");
                if scroll && index == 0 {
                    unsafe {
                        let list = CreateWindowExW(
                            Default::default(),
                            w!("LISTBOX"),
                            w!("Parity scroll list"),
                            WS_CHILD
                                | WS_VISIBLE
                                | WS_VSCROLL
                                | WINDOW_STYLE(LBS_NOINTEGRALHEIGHT as u32),
                            10,
                            10,
                            180,
                            80,
                            Some(hwnd),
                            None,
                            None,
                            None,
                        )
                        .expect("create scroll fixture");
                        for row in 0..80 {
                            let text =
                                windows::core::HSTRING::from(format!("Fixture row {row:02}"));
                            SendMessageW(
                                list,
                                LB_ADDSTRING,
                                None,
                                Some(windows::Win32::Foundation::LPARAM(text.as_ptr() as isize)),
                            );
                        }
                    }
                }
                if password && index == 1 {
                    unsafe {
                        CreateWindowExW(
                            Default::default(),
                            w!("EDIT"),
                            w!("test secret"),
                            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_PASSWORD as u32),
                            10,
                            10,
                            120,
                            25,
                            Some(hwnd),
                            None,
                            None,
                            None,
                        )
                    }
                    .expect("create password fixture control");
                }
                unsafe {
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                }
                handles.push(hwnd);
            }
            ready.send(()).unwrap();
            while stopped.try_recv().is_err() {
                let mut message = MSG::default();
                while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                    unsafe {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            for hwnd in handles {
                unsafe {
                    DestroyWindow(hwnd).unwrap();
                }
            }
        });
        started
            .recv_timeout(Duration::from_secs(5))
            .expect("fixture started");
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

#[test]
#[ignore = "scrolls only a disposable listbox through the production UIA adapter"]
fn owned_window_semantic_scroll_moves_only_the_selected_control() {
    use desk_agent_protocol::computer_use::UiInspectParams;
    use windows::Win32::UI::Accessibility::{IUIAutomationScrollPattern, UIA_ScrollPatternId};
    let foreground = unsafe { GetForegroundWindow() };
    let fixture = Fixture::with_controls(false, true);
    let native = application_by_process(std::process::id()).unwrap();
    let selected = super::super::computer_use_broker::ObservedApplication {
        window_handle: native.window_handle,
        process_id: native.process_id,
        image_path: native.image_path.clone(),
        process_started_at: Some(native.process_started_at),
    };
    let params = UiInspectParams {
        root: None,
        scope: UiInspectScope::All,
        allow_unfiltered: true,
        query: None,
        overview: false,
        element_only: false,
        max_depth: 16,
        max_nodes: 300,
        max_bytes: 262144,
    };
    let tree = collect_application_selection(&selected, &params, None).unwrap();
    let target = tree
        .nodes
        .iter()
        .find(|node| {
            node.supported_actions
                .contains(&UiSemanticActionKind::Scroll)
        })
        .expect("observed scrollable listbox")
        .fingerprint
        .clone();
    let read = || {
        let target = target.clone();
        super::super::native_ui_identity::run(move || {
            let element = retained_element(&target, std::process::id(), native.process_started_at)?;
            let pattern: IUIAutomationScrollPattern =
                unsafe { element.GetCurrentPatternAs(UIA_ScrollPatternId) }.unwrap();
            Ok(unsafe { pattern.CurrentVerticalScrollPercent() }.unwrap())
        })
        .unwrap()
    };
    let before = read();
    assert!(
        preflight_action(
            native.process_id,
            &native.image_path,
            &target,
            &UiSemanticAction::Scroll {
                horizontal: 1,
                vertical: 0
            }
        )
        .is_err()
    );
    assert_eq!(before, read());
    let action = UiSemanticAction::Scroll {
        horizontal: 0,
        vertical: 2,
    };
    preflight_action(native.process_id, &native.image_path, &target, &action).unwrap();
    apply_action(native.process_id, &native.image_path, &target, &action).unwrap();
    assert!(read() > before, "fresh scroll position must move down");
    assert_eq!(unsafe { GetForegroundWindow() }, foreground);
    drop(fixture);
    assert!(apply_action(native.process_id, &native.image_path, &target, &action).is_err());
}

#[test]
#[ignore = "uses only disposable windows through the production broker and PNG collector"]
fn owned_window_broker_returns_png_for_the_selected_reference() {
    broker_capture(false);
}

#[test]
#[ignore = "creates a disposable password control and verifies capture rejection"]
fn owned_window_broker_rejects_protected_controls() {
    broker_capture(true);
}

fn broker_capture(password: bool) {
    use super::super::computer_use_broker::ComputerUseBroker;
    use crate::model::settings::ComputerUseSettings;
    use desk_agent_protocol::ScreenCaptureParams;
    use desk_agent_protocol::computer_use::{DesktopSessionInspectParams, UiInspectParams};
    use std::sync::Arc;
    let fixture = Fixture::with_password(password);
    let application = application_by_process(std::process::id()).unwrap();
    let ceiling = ComputerUseSettings {
        enabled: true,
        observe: true,
        allowed_application_paths: vec![application.image_path],
        ..Default::default()
    };
    let broker = Arc::new(ComputerUseBroker::new());
    let session = broker
        .inspect_desktop_session(
            &DesktopSessionInspectParams {
                include_active_application: false,
            },
            &ceiling,
        )
        .unwrap();
    let mut params = UiInspectParams {
        root: Some(session.session),
        scope: Default::default(),
        allow_unfiltered: true,
        query: None,
        overview: false,
        element_only: false,
        max_depth: 16,
        max_nodes: 300,
        max_bytes: 262144,
    };
    let catalog = broker.inspect_desktop_ui(&params, &ceiling).unwrap();
    assert_eq!(catalog.nodes.len(), 1);
    params.root = Some(catalog.nodes[0].object_ref.clone());
    broker.note_external_input();
    let tree = broker.inspect_desktop_ui(&params, &ceiling).unwrap();
    let window = tree
        .owner_selectable_windows
        .iter()
        .find(|window| window.title.as_deref() == Some("Parity fixture beta"))
        .expect("selected beta window")
        .object_ref
        .clone();
    let capture = ScreenCaptureParams {
        window: Some(window.clone()),
        display: None,
    };
    if password {
        assert!(matches!(
            broker.acquire_screen_capture_permit(&capture, "fixture-display"),
            Err(AgentError {
                kind: AgentErrorKind::PermissionDenied,
                message, ..
            }) if message.contains("protected controls")
        ));
        return;
    }
    let permit = broker
        .acquire_screen_capture_permit(&capture, "fixture-display")
        .unwrap();
    let output = super::super::collectors::screen_capture::collect(
        &capture,
        &Default::default(),
        permit.window_target(),
    )
    .unwrap();
    permit.validate_window_after_capture().unwrap();
    assert_eq!(output.window, Some(window));
    assert_eq!(&output.image[..8], b"\x89PNG\r\n\x1a\n");
    assert!(output.width > 240 && output.width <= 280);
    assert!(output.window_geometry.is_some());
    drop(fixture);
    assert!(permit.validate_window_after_capture().is_err());
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
#[ignore = "creates two disposable windows on an interactive Windows desktop"]
fn owned_windows_are_read_together_and_selected_independently() {
    exercise_owned_windows(false);
}

#[test]
#[ignore = "captures only a disposable fixture window using WGC"]
fn owned_window_capture_uses_selected_window_and_rejects_closed_target() {
    exercise_owned_windows(true);
}

fn exercise_owned_windows(capture: bool) {
    let fixture = Fixture::start();
    let native = application_by_process(std::process::id()).unwrap();
    let application = super::super::computer_use_broker::ObservedApplication {
        window_handle: 0,
        process_id: native.process_id,
        image_path: native.image_path.clone(),
        process_started_at: Some(native.process_started_at),
    };
    let params = desk_agent_protocol::computer_use::UiInspectParams {
        root: None,
        scope: Default::default(),
        allow_unfiltered: true,
        query: None,
        overview: false,
        element_only: false,
        max_depth: 8,
        max_nodes: 128,
        max_bytes: 128 * 1024,
    };
    let tree = collect_application_selection(&application, &params, None).unwrap();
    let alpha = tree
        .nodes
        .iter()
        .find(|n| n.name.as_deref() == Some("Parity fixture alpha"))
        .expect("first window");
    let beta = tree
        .nodes
        .iter()
        .find(|n| n.name.as_deref() == Some("Parity fixture beta"))
        .expect("second window");
    assert_ne!(alpha.fingerprint, beta.fingerprint);
    assert_eq!(
        alpha.window_fingerprint.as_deref(),
        Some(alpha.fingerprint.as_str())
    );
    assert_eq!(
        beta.window_fingerprint.as_deref(),
        Some(beta.fingerprint.as_str())
    );
    let beta_id = beta.fingerprint.clone();
    let target = application_for_element(native.process_id, &native.image_path, &beta_id).unwrap();
    let selected = super::super::computer_use_broker::ObservedApplication {
        window_handle: target.window_handle,
        process_id: target.process_id,
        image_path: target.image_path,
        process_started_at: Some(target.process_started_at),
    };
    let tree = collect_application_selection(&selected, &params, Some(&beta_id)).unwrap();
    assert!(
        tree.nodes
            .iter()
            .any(|n| n.name.as_deref() == Some("Parity fixture beta"))
    );
    assert!(
        !tree
            .nodes
            .iter()
            .any(|n| n.name.as_deref() == Some("Parity fixture alpha"))
    );
    if capture {
        use desk_capture_engine::image_capture::windows_window_capture::{
            WindowsWindowCaptureTarget, capture_independent_window,
        };
        use desk_capture_engine::model::image_capture::ImageInfo;
        let target = WindowsWindowCaptureTarget {
            window_handle: selected.window_handle,
            host_process_id: native.process_id,
            host_process_started_at: native.process_started_at,
        };
        let frame = capture_independent_window(&target).expect("capture selected fixture window");
        assert!(frame.get_width() > 240 && frame.get_width() <= 280);
        assert!(frame.get_height() > 100 && frame.get_height() <= 160);
        assert_eq!(
            frame.get_data().len(),
            (frame.get_width() * frame.get_height() * 4) as usize
        );
        unsafe {
            let _ = ShowWindow(
                windows::Win32::Foundation::HWND(selected.window_handle as *mut _),
                SW_SHOWMINNOACTIVE,
            );
        }
        assert!(capture_independent_window(&target).is_err());
        drop(fixture);
        assert!(capture_independent_window(&target).is_err());
    } else {
        drop(fixture);
    }
    assert!(application_for_element(native.process_id, &native.image_path, &beta_id).is_err());
}
