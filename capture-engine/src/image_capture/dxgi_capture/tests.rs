use std::path::{Path, PathBuf};
use std::thread;

use crate::model::image_capture::{CaptureRequest, CursorCaptureMode};
use desk_utils::logs::init_logs;
use log::LevelFilter;
use std::sync::{Barrier, Once};
use windows::Win32::Foundation::LPARAM;
use windows::Win32::System::StationsAndDesktops::{
    CloseWindowStation, CreateDesktopW, EnumDesktopsW, EnumWindowStationsW,
    GetProcessWindowStation, GetThreadDesktop, HWINSTA, OpenDesktopW, OpenWindowStationW,
    SwitchDesktop,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::IsUserAnAdmin;
use windows::Win32::UI::WindowsAndMessaging::{MB_OK, MessageBoxW};
use windows_core::w;
use yuv::bgra_to_rgba;

use super::*;

static INIT: Once = Once::new();

// -----------------------------------------------------------------
// Pure-function tests (cross-adapter enumeration helpers).
// No DXGI fixtures needed.
// -----------------------------------------------------------------

/// Smoke test for `FrameAcquisitionResult::Rebuild`: the new
/// variant exists, is constructible, and is reachable via
/// exhaustive match. Compiler-enforced exhaustiveness on
/// downstream `match acq_result { ... }` is the real guarantee;
/// this test exists so a future reader who deletes the variant
/// also has to update an explicit assertion.
#[test]
fn screen_output_rebuild_variant_can_be_matched() {
    let r: FrameAcquisitionResult<'_> = FrameAcquisitionResult::Rebuild;
    let tag = match r {
        FrameAcquisitionResult::Rebuild => "rebuild",
        FrameAcquisitionResult::NoContentChange => "nochange",
        FrameAcquisitionResult::ContentFrame(_) => "content",
    };
    assert_eq!(tag, "rebuild");
}

// -----------------------------------------------------------------
// Cursor fingerprint (M1) — size-aware identity.
// -----------------------------------------------------------------

/// Two `Shape` fingerprints with the same `id` but different
/// `screen_width` are not equal. This guarantees a mid-session
/// resolution change re-emits cursor metadata even when the
/// cursor pixel hash is unchanged — otherwise the front-end's
/// stale `screen_width` would mis-scale the cursor sprite.
#[test]
fn dxgi_fingerprint_differs_on_screen_width_change() {
    let a = DxgiCursorFingerprint::Shape {
        id: 0xabcd,
        screen_width: 1920,
        screen_height: 1080,
    };
    let b = DxgiCursorFingerprint::Shape {
        id: 0xabcd,
        screen_width: 2560,
        screen_height: 1080,
    };
    assert_ne!(a, b);
}

#[test]
fn dxgi_fingerprint_differs_on_screen_height_change() {
    let a = DxgiCursorFingerprint::Shape {
        id: 0xabcd,
        screen_width: 1920,
        screen_height: 1080,
    };
    let b = DxgiCursorFingerprint::Shape {
        id: 0xabcd,
        screen_width: 1920,
        screen_height: 1440,
    };
    assert_ne!(a, b);
}

#[test]
fn dxgi_fingerprint_equal_when_all_fields_match() {
    let a = DxgiCursorFingerprint::Shape {
        id: 0xabcd,
        screen_width: 1920,
        screen_height: 1080,
    };
    let b = DxgiCursorFingerprint::Shape {
        id: 0xabcd,
        screen_width: 1920,
        screen_height: 1080,
    };
    assert_eq!(a, b);
}

/// `Embedded` is the third state. It must compare unequal to
/// both `Hidden` and any `Shape` so transitions between
/// hardware-cursor and software-cursor modes always emit a
/// fresh `CursorSyncData` (the front-end uses the new payload
/// to toggle its local CSS cursor visibility).
#[test]
fn dxgi_fingerprint_embedded_differs_from_hidden_and_shape() {
    let embedded = DxgiCursorFingerprint::Embedded;
    let hidden = DxgiCursorFingerprint::Hidden;
    let shape = DxgiCursorFingerprint::Shape {
        id: 1,
        screen_width: 1920,
        screen_height: 1080,
    };
    assert_ne!(embedded, hidden);
    assert_ne!(embedded, shape);
    assert_ne!(hidden, shape);
}

// -----------------------------------------------------------------
// Embedded-cursor detection (M3) — WebRTC heuristic.
// -----------------------------------------------------------------

fn frame_info_for_embedded_test(
    last_mouse_update_time: i64,
    pointer_visible: bool,
) -> DXGI_OUTDUPL_FRAME_INFO {
    let mut frame_info: DXGI_OUTDUPL_FRAME_INFO = unsafe { std::mem::zeroed() };
    frame_info.LastMouseUpdateTime = last_mouse_update_time;
    frame_info.PointerPosition.Visible = windows_core::BOOL(if pointer_visible { 1 } else { 0 });
    frame_info
}

/// Software-cursor frame: the OS reports a fresh pointer-position
/// update (LastMouseUpdateTime != 0) but tags it not-visible
/// (i.e. there's no separate hardware pointer plane to render),
/// meaning the cursor pixel is already composited into the
/// desktop image.
#[test]
fn frame_contains_embedded_cursor_true_when_invisible_and_mouse_update() {
    let f = frame_info_for_embedded_test(0x1234, false);
    assert!(frame_contains_embedded_cursor(&f));
}

/// Hardware-cursor frame: pointer is visible as a separate
/// overlay plane, so it is not part of the acquired desktop
/// image. The fact that LastMouseUpdateTime is non-zero only
/// means the OS has fresh pointer position info to deliver.
#[test]
fn frame_contains_embedded_cursor_false_when_visible_with_update() {
    let f = frame_info_for_embedded_test(0x1234, true);
    assert!(!frame_contains_embedded_cursor(&f));
}

/// No pointer-position update this frame — the duplication API
/// gives no signal one way or the other, so we cannot claim the
/// cursor is embedded. Returning false here keeps the
/// previous-frame state machine driven by the next genuine
/// update.
#[test]
fn frame_contains_embedded_cursor_false_when_no_mouse_update() {
    let f = frame_info_for_embedded_test(0, false);
    assert!(!frame_contains_embedded_cursor(&f));
}

/// Both predicates inverted: no update *and* pointer marked
/// visible. Equivalent to the no-update case for our purposes.
#[test]
fn frame_contains_embedded_cursor_false_when_no_update_and_visible() {
    let f = frame_info_for_embedded_test(0, true);
    assert!(!frame_contains_embedded_cursor(&f));
}

fn luid(lo: u32, hi: i32) -> LUID {
    LUID {
        LowPart: lo,
        HighPart: hi,
    }
}

fn make_names(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn find_device_name_index_finds_match() {
    let names = make_names(&[r"\\.\DISPLAY1", r"\\.\DISPLAY7"]);
    assert_eq!(
        find_device_name_index(&names, r"\\.\DISPLAY7").expect("match"),
        1
    );
    assert_eq!(
        find_device_name_index(&names, r"\\.\DISPLAY1").expect("match"),
        0
    );
}

/// Empty `video_device_name` is a legal-but-unselected state on
/// fresh installs — the daemon must not silently fall back to
/// "device index 0" on this path. Hard error so the caller (the
/// capture-engine factory, ultimately) surfaces a structured
/// failure to the worker / pipeline. The frontend keeps users from
/// hitting this in practice by gating submit on a non-empty name.
#[test]
fn find_device_name_index_returns_invalid_params_when_empty_string() {
    let names = make_names(&[r"\\.\DISPLAY1"]);
    let err = find_device_name_index(&names, "").expect_err("empty string must error");
    let msg = format!("{}", err);
    assert!(
        msg.contains("video_device_name is empty"),
        "error message must call out the empty selection: {}",
        msg
    );
}

#[test]
fn find_device_name_index_returns_invalid_params_when_no_match() {
    let names = make_names(&[r"\\.\DISPLAY1", r"\\.\DISPLAY7"]);
    let err =
        find_device_name_index(&names, r"\\.\DISPLAY99").expect_err("unknown name must error");
    let msg = format!("{}", err);
    // The Debug formatter double-escapes backslashes, so the
    // assertion targets the human-recognisable suffix that is
    // stable across Display/Debug rendering.
    assert!(
        msg.contains("DISPLAY99") && msg.contains("DISPLAY1") && msg.contains("DISPLAY7"),
        "error message must include the requested name and the enumerated list: {}",
        msg
    );
}

/// A not-found resolve must surface enough context for the user
/// to recover: the requested name, the enumerated list (so a
/// hot-plug / detached / asleep monitor is obvious), and a WGC
/// fallback pointer for the rare cases where the WGC enumerator
/// happens to list a device the DXGI enumerator does not. The
/// frontend uses this string verbatim from the worker log only
/// for diagnostics — actual fallback is the user's job once they
/// see the suggestion.
#[test]
fn dxgi_select_by_name_not_found_emits_actionable_hint() {
    let names = make_names(&[r"\\.\DISPLAY1"]);
    let err = find_device_name_index(&names, r"\\.\DISPLAY99").expect_err("not found");
    let msg = format!("{}", err);
    // The Debug formatter double-escapes backslashes, so the
    // assertions target the human-recognisable suffix that is
    // stable across Display/Debug rendering (mirrors the
    // convention used by the other not-found tests in this file).
    assert!(
        msg.contains("DISPLAY99"),
        "error message must echo the requested device name: {}",
        msg
    );
    assert!(
        msg.contains("DISPLAY1"),
        "error message must list the enumerated device names: {}",
        msg
    );
    assert!(
        msg.contains("hot-plug") || msg.contains("re-open the desktop dialog"),
        "error message must hint at hot-plug / detached-monitor recovery: {}",
        msg
    );
    assert!(
        msg.contains("WGC"),
        "error message must mention WGC as the alternative backend: {}",
        msg
    );
}

#[test]
fn order_adapters_default_in_middle_promoted_to_front() {
    let a = luid(1, 0);
    let b = luid(2, 0);
    let c = luid(3, 0);
    let got = order_adapters_by_default_luid(Some(b), &[a, b, c]);
    assert_eq!(got, vec![1, 0, 2]);
}

#[test]
fn order_adapters_default_not_found_keeps_factory_order() {
    let a = luid(1, 0);
    let b = luid(2, 0);
    let c = luid(3, 0);
    let unknown = luid(99, 0);
    let got = order_adapters_by_default_luid(Some(unknown), &[a, b, c]);
    assert_eq!(got, vec![0, 1, 2]);
}

#[test]
fn order_adapters_default_is_none_keeps_factory_order() {
    let a = luid(1, 0);
    let b = luid(2, 0);
    let c = luid(3, 0);
    let got = order_adapters_by_default_luid(None, &[a, b, c]);
    assert_eq!(got, vec![0, 1, 2]);
}

#[test]
fn order_adapters_single_adapter_is_idempotent() {
    let a = luid(7, 1);
    assert_eq!(order_adapters_by_default_luid(Some(a), &[a]), vec![0]);
    assert_eq!(order_adapters_by_default_luid(None, &[a]), vec![0]);
}

#[test]
fn order_adapters_empty_input_returns_empty() {
    let got = order_adapters_by_default_luid(Some(luid(1, 0)), &[]);
    assert!(got.is_empty());
}

// -----------------------------------------------------------------
// Integration smoke tests for `enumerate_all_outputs` — gated on
// Windows + #[ignore] so they only run on a real DXGI host via
// `cargo test -- --ignored`.
// -----------------------------------------------------------------

#[cfg(target_os = "windows")]
#[test]
#[ignore]
fn enumerate_all_outputs_returns_nonempty_on_windows() {
    initialize();
    let outputs = enumerate_all_outputs().expect("enumerate_all_outputs");
    assert!(
        !outputs.is_empty(),
        "at least one DXGI output should exist on a Windows host"
    );
    for o in &outputs {
        log::info!(
            "[enum] adapter_index={} local_output_index={} adapter='{}' display='{}'",
            o.adapter_index,
            o.local_output_index,
            adapter_name_from_desc(&o.adapter_desc),
            from_dxgi_output_desc(&o.desc).device_name,
        );
    }
}

#[cfg(target_os = "windows")]
#[test]
#[ignore]
fn enumerate_all_outputs_finds_lcxl_idd_when_present() {
    initialize();
    let outputs = enumerate_all_outputs().expect("enumerate_all_outputs");
    let lcxl = outputs.iter().find(|o| {
        let info = from_dxgi_output_desc(&o.desc);
        info.display_device_name
            .as_deref()
            .map(|s| s.to_ascii_lowercase().contains("lcxl"))
            .unwrap_or(false)
    });
    assert!(
        lcxl.is_some(),
        "expected to find an output whose EnumDisplayDevicesW DeviceString contains 'Lcxl'. \
             Confirm `cargo run -p poc-indirect-display -- create-device` is running before running this test."
    );
}

pub fn initialize() {
    INIT.call_once(|| {
        // initialization code here
        let _ = init_logs(LevelFilter::Debug);
        let result = ScreenRecordManager::set_thread_input_desktop();
        log::info!("set thread desktop result: {:?}", result);
    });
}

/// Save screenshot to file
fn save_screenshot_to_file(
    capture: &mut DxgiImageCapture,
    bmp_path: &Path,
) -> Result<(), CaptureError> {
    let capture_result = capture.capture(CaptureRequest {
        cursor_mode: CursorCaptureMode::RenderInFrame,
    })?;
    let frame = capture_result.image;
    log::info!("frame_buffer.len={}", frame.get_data().len());
    let mut rgb_data = vec![0u8; frame.get_data().len()];
    let rgb_data_array = rgb_data.as_mut_slice();

    let src_stride = frame.get_width() * 4;
    let dst_stride = frame.get_width() * 4;
    // convert bgra to rgba
    bgra_to_rgba(
        frame.get_data(),
        src_stride,
        rgb_data_array,
        dst_stride,
        frame.get_width(),
        frame.get_height(),
    )?;
    image::save_buffer(
        bmp_path,
        rgb_data_array,
        frame.get_width(),
        frame.get_height(),
        image::ExtendedColorType::Rgba8,
    )
    .unwrap();
    log::info!("saved screenshot to {}", bmp_path.to_string_lossy());
    Ok(())
}

/// Hardware + interactive-desktop dependent: needs a live D3D11
/// adapter and an attached display. Fails on headless CI / RDP-only
/// sessions with `DXGI_ERROR_DEVICE_HUNG` (0x887A0005) once another
/// D3D test in the suite has already consumed the device. Run
/// manually with `cargo test -- --ignored test_screen` on a
/// machine with a GPU + monitor.
#[test]
#[ignore]
fn test_screen() -> Result<(), CaptureError> {
    initialize();
    let settings = DeskSettings::default();
    let mut capture = DxgiImageCapture::new(&settings)?;

    let list = DxgiImageOutputEnumerator::new().get_output_list()?;
    assert!(!list.is_empty());

    let tmp_dir = PathBuf::from("sample/screenshot");
    std::fs::create_dir_all(tmp_dir.as_path())?;

    for i in 0..10 {
        let name = tmp_dir.join(format!("screenshot_{}.bmp", i));
        save_screenshot_to_file(&mut capture, name.as_path())?;
    }
    //std::fs::remove_dir_all(tmp_dir.as_path())?;

    Ok(())
}

unsafe extern "system" fn enum_proc(
    param0: windows_core::PCWSTR,
    param1: LPARAM,
) -> windows_core::BOOL {
    let result = unsafe { param0.to_string() };
    let windows_station_list_pointer = param1.0 as *mut Vec<String>;
    if let Ok(name) = result {
        log::info!("add: {}", name);
        let windows_station_list = unsafe { windows_station_list_pointer.as_mut().unwrap() };
        windows_station_list.push(name);
    } else if let Err(e) = result {
        log::error!("failed to add: {}", e);
    }

    windows_core::BOOL::from(true)
}

fn list_desktop_by_station_handle(handle: HWINSTA) {
    let mut desktop_list = Vec::<String>::new();
    let desktop_list_pointer = &raw mut desktop_list;
    let lparam = LPARAM(desktop_list_pointer as isize);
    let enum_result = unsafe { EnumDesktopsW(Some(handle), Some(enum_proc), lparam) };
    log::info!("EnumDesktopsW result: {:?}", enum_result);
    log::info!("desktop_list: {:?}", desktop_list);

    let mut settings = DeskSettings::default();

    for desktop_name in desktop_list {
        let mut desktop_name_utf16: Vec<u16> = desktop_name.encode_utf16().collect();
        // add null terminator to the station name utf16
        desktop_name_utf16.push(0);
        let desktop_name_ptr = windows::core::PCWSTR::from_raw(desktop_name_utf16.as_ptr());

        let hdesk_result = unsafe {
            OpenDesktopW(
                desktop_name_ptr,
                DESKTOP_CONTROL_FLAGS(0),
                true,
                GENERIC_ALL.0,
            )
        };
        let hdesk = match hdesk_result {
            Ok(hdesk) => hdesk,
            Err(e) => {
                log::error!("Failed to open desktop {}: {}", desktop_name, e);
                continue;
            }
        };
        let result = unsafe { SetThreadDesktop(hdesk) };

        let _ = unsafe { CloseDesktop(hdesk) };

        if let Err(e) = result {
            log::error!("Failed to set thread desktop {}: {}", desktop_name, e);
            continue;
        }

        let enumerator = DxgiImageOutputEnumerator::new();
        let list_result = enumerator.get_output_list();
        if let Err(e) = list_result {
            log::error!("Failed to get output list {}: {}", desktop_name, e);
            continue;
        }

        let output_list = list_result.unwrap();
        log::info!(
            "Output list for desktop {}: {:?}",
            desktop_name,
            output_list
        );
        drop(enumerator);
        for output in &output_list {
            settings.video_device_name = output.device_name.clone();
            let capture_result = DxgiImageCapture::new(&settings);
            if let Err(e) = capture_result {
                log::error!("Failed to get screen output {}: {}", desktop_name, e);
                continue;
            }

            let mut capture = capture_result.unwrap();
            // first frame is black, skip it
            capture
                .capture(CaptureRequest {
                    cursor_mode: CursorCaptureMode::Disable,
                })
                .unwrap();

            let tmp_dir = PathBuf::from("sample");
            let sanitized_name: String = output
                .device_name
                .chars()
                .map(|c| if c == '\\' || c == '.' { '_' } else { c })
                .collect();
            let name = tmp_dir.join(format!(
                "screenshot_{}_{}.bmp",
                desktop_name, sanitized_name
            ));

            save_screenshot_to_file(&mut capture, name.as_path()).unwrap();
        }
    }
}
#[test]
fn test_windows_api() -> Result<(), CaptureError> {
    initialize();
    let is_admin = unsafe { IsUserAnAdmin() };

    log::info!("is user an admin: {}", is_admin.as_bool());
    let mut windows_station_list = Vec::<String>::new();
    let windows_station_list_pointer = &raw mut windows_station_list;
    let lparam = LPARAM(windows_station_list_pointer as isize);
    let result = unsafe { EnumWindowStationsW(Some(enum_proc), lparam) };
    log::info!("EnumWindowStationsW result: {:?}", result);
    log::info!("windows_station_list: {:?}", windows_station_list);

    for station in &windows_station_list {
        log::info!("station: {}", station);
        let mut station_name_utf16: Vec<u16> = station.encode_utf16().collect();
        // add null terminator to the station name utf16
        station_name_utf16.push(0);
        let station_name_ptr = windows::core::PCWSTR::from_raw(station_name_utf16.as_ptr());
        let open_result = unsafe { OpenWindowStationW(station_name_ptr, true, GENERIC_ALL.0) };

        if let Ok(handle) = open_result {
            list_desktop_by_station_handle(handle);

            let close_result = unsafe { CloseWindowStation(handle) };
            log::info!("CloseWindowStation result: {:?}", close_result);
        } else if let Err(e) = open_result {
            log::error!(
                "OpenWindowStationW error, station: {}, error: {}",
                station,
                e
            );
        }
    }
    let result = unsafe { GetProcessWindowStation() };
    if let Ok(handle) = result {
        log::info!("GetProcessWindowStation handle: {:?}", handle);
        list_desktop_by_station_handle(handle);
    } else if let Err(e) = result {
        log::error!("GetProcessWindowStation error: {}", e);
    }

    Ok(())
}

// Disabled by default: hangs on `Barrier::wait` waiting for an
// interactive desktop switch that doesn't happen under
// `cargo test --workspace`. Re-enable with `cargo test -- --ignored`
// when investigating desktop-switch behaviour manually.
#[test]
#[ignore]
fn test_switch_desktop() -> Result<(), CaptureError> {
    initialize();
    let current_thread_id = unsafe { GetCurrentThreadId() };
    log::info!("Current thread id: {}", current_thread_id);

    let h_old = unsafe { GetThreadDesktop(current_thread_id) }?;
    let barrier = Arc::new(Barrier::new(2));
    let b = barrier.clone();
    let thread_handle = thread::spawn(move || {
        let h_old = unsafe { GetThreadDesktop(current_thread_id) }.unwrap();
        log::info!(
            "Get thread desktop handle: {:?}, from thread id: {}",
            h_old,
            current_thread_id
        );
        unsafe { SetThreadDesktop(h_old) }.unwrap();
        let settings = DeskSettings::default();
        let mut capture = DxgiImageCapture::new(&settings).unwrap();

        log::info!("Wait for barrier");
        b.wait();
        thread::sleep(std::time::Duration::from_secs(5)); // wait
        log::info!("Start to capture screen");
        let screent_output_result = capture.capture(CaptureRequest {
            cursor_mode: CursorCaptureMode::RenderInFrame,
        });
        if let Err(e) = screent_output_result {
            log::error!("Failed to get screen output: {}", e);

            let mut capture = DxgiImageCapture::new(&settings).unwrap();
            capture
                .capture(CaptureRequest {
                    cursor_mode: CursorCaptureMode::RenderInFrame,
                })
                .unwrap();

            let tmp_dir = PathBuf::from("sample");
            let name = tmp_dir.join("switch_desktop_screenshot_retry.bmp".to_string());

            save_screenshot_to_file(&mut capture, name.as_path()).unwrap();
            return;
        }
        screent_output_result.unwrap();

        let tmp_dir = PathBuf::from("sample");
        let name = tmp_dir.join("switch_desktop_screenshot.bmp".to_string());

        save_screenshot_to_file(&mut capture, name.as_path()).unwrap();
    });

    log::info!("Old desktop handle: {:?}", h_old);
    // add null terminator to the station name utf16
    let desktop_name_ptr = w!("Test");
    barrier.wait();
    let h_new = unsafe {
        CreateDesktopW(
            desktop_name_ptr,
            windows::core::PCWSTR::null(),
            None,
            DESKTOP_CONTROL_FLAGS(0),
            GENERIC_ALL.0,
            None,
        )
    }?;
    log::info!("New desktop handle: {:?}", h_new);
    unsafe { SetThreadDesktop(h_new) }?;
    unsafe { SwitchDesktop(h_new) }?;

    let text_ptr = w!("成功!");
    let caption_ptr = w!("测试!");

    unsafe { MessageBoxW(None, text_ptr, caption_ptr, MB_OK) };
    unsafe { SwitchDesktop(h_old) }?;
    let _ = unsafe { CloseDesktop(h_new) };

    // wait for the thread to finish
    let _ = thread_handle.join();
    Ok(())
}
