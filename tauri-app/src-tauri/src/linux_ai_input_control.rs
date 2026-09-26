//! Local consent window and connection ownership for GNOME input control.
use lcxl_remote_desk_server::worker::agent::{InputControlCancellation, NativeInputControlClient};
use serde::Serialize;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tauri::{Manager, WebviewUrl, WebviewWindow};

const LABEL: &str = "linux-ai-input-control";
#[derive(Default)]
struct State {
    endpoint: Option<PathBuf>,
    endpoint_revision: u64,
    epoch: u64,
    phase: &'static str,
    duration_secs: Option<u16>,
    accept_partial: bool,
    report: Option<[usize; 3]>,
    message: Option<String>,
    cancel: Option<Arc<InputControlCancellation>>,
}
static STATE: OnceLock<Mutex<State>> = OnceLock::new();
fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| Mutex::new(State::default()))
}
#[derive(Serialize)]
pub(crate) struct View {
    revision: u64,
    available: bool,
    phase: &'static str,
    duration_secs: Option<u16>,
    accept_partial: bool,
    report: Option<[usize; 3]>,
    message: Option<String>,
    locale: String,
}
fn snapshot() -> View {
    let s = state().lock().unwrap();
    View {
        revision: s.epoch,
        available: s.endpoint.is_some(),
        phase: s.phase,
        duration_secs: s.duration_secs,
        accept_partial: s.accept_partial,
        report: s.report,
        message: s.message.clone(),
        locale: lcxl_remote_desk_server::locale::current_locale(),
    }
}
fn revoke(s: &mut State) {
    s.epoch = s
        .epoch
        .checked_add(1)
        .expect("Local input generation exhausted");
    if let Some(cancel) = s.cancel.take() {
        cancel.cancel();
    }
    s.phase = "idle";
    s.duration_secs = None;
    s.accept_partial = false;
    s.report = None;
    s.message = None;
}
pub(crate) fn endpoint(revision: u64, path: Option<String>) {
    let path = path.map(PathBuf::from);
    let mut s = state().lock().unwrap();
    if revision < s.endpoint_revision {
        return;
    }
    s.endpoint_revision = revision;
    if s.endpoint != path {
        revoke(&mut s);
        s.endpoint = path;
    }
}
pub(crate) fn disconnect() {
    let mut s = state().lock().unwrap();
    revoke(&mut s);
    s.endpoint = None;
    s.endpoint_revision = 0;
}
fn trusted_url(url: &url::Url) -> bool {
    let local = (url.scheme() == "tauri" && url.host_str() == Some("localhost"))
        || (url.scheme() == "http" && url.host_str() == Some("tauri.localhost"));
    local
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/linux-ai-input.html"
}
fn authorized(window: &WebviewWindow) -> Result<(), String> {
    let url = window.url().map_err(|e| e.to_string())?;
    if window.label() != LABEL || !trusted_url(&url) {
        return Err("Input control requires the local consent window".into());
    }
    Ok(())
}
pub(crate) fn show(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let result =
        tauri::WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("linux-ai-input.html".into()))
            .title(rust_i18n::t!("linux_ai_input_title"))
            .inner_size(500.0, 530.0)
            .resizable(false)
            .on_navigation(trusted_url)
            .build();
    match result {
        Ok(window) => window.on_window_event(|event| {
            if matches!(event, tauri::WindowEvent::Destroyed) {
                revoke(&mut state().lock().unwrap());
            }
        }),
        Err(error) => log::warn!("Could not open local AI input control: {error}"),
    }
}
#[tauri::command]
pub(crate) fn linux_ai_input_status(window: WebviewWindow) -> Result<View, String> {
    authorized(&window)?;
    Ok(snapshot())
}
#[tauri::command]
pub(crate) fn linux_ai_input_start(
    window: WebviewWindow,
    seconds: u16,
    accept_partial: bool,
) -> Result<View, String> {
    authorized(&window)?;
    if !(1..=300).contains(&seconds) {
        return Err("Duration must be 1..300 seconds".into());
    }
    let (path, epoch) = {
        let mut s = state().lock().unwrap();
        if matches!(s.phase, "starting" | "active" | "stopping") {
            return Err("Input control is already in progress".into());
        }
        let path = s
            .endpoint
            .clone()
            .ok_or("No input endpoint for this host/session")?;
        revoke(&mut s);
        s.phase = "starting";
        s.duration_secs = Some(seconds);
        s.accept_partial = accept_partial;
        (path, s.epoch)
    };
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        if state().lock().unwrap().epoch != epoch {
            return;
        }
        let result = (|| -> std::io::Result<()> {
            let client = NativeInputControlClient::connect(&path, seconds, accept_partial)?;
            let cancel = Arc::new(client.cancellation()?);
            {
                let mut s = state().lock().unwrap();
                if s.epoch != epoch {
                    return Ok(());
                }
                let report = client.report();
                s.report = Some([report.grabbed, report.failed, report.skipped]);
                s.cancel = Some(cancel);
                s.phase = "active";
            }
            client.wait()
        })();
        let mut s = state().lock().unwrap();
        if s.epoch == epoch {
            s.cancel = None;
            match result {
                Ok(()) => {
                    s.phase = "released";
                    s.message = None;
                }
                Err(error) => {
                    s.phase = "error";
                    s.message = Some(error.to_string());
                }
            }
        }
    });
    Ok(snapshot())
}
#[tauri::command]
pub(crate) async fn linux_ai_input_stop(window: WebviewWindow) -> Result<View, String> {
    authorized(&window)?;
    let pending = {
        let mut s = state().lock().unwrap();
        match s.cancel.clone() {
            Some(cancel) => {
                s.phase = "stopping";
                Some((cancel, s.epoch))
            }
            None => {
                revoke(&mut s);
                None
            }
        }
    };
    if let Some((cancel, epoch)) = pending {
        let result = tauri::async_runtime::spawn_blocking(move || cancel.request_stop())
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r.map_err(|e| e.to_string()));
        if let Err(error) = result {
            let mut s = state().lock().unwrap();
            if s.epoch == epoch && s.phase == "stopping" {
                s.cancel = None;
                s.phase = "error";
                s.message = Some(error);
            }
        }
    }
    Ok(snapshot())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_the_bundled_consent_asset_can_invoke_control() {
        for (value, allowed) in [
            ("tauri://localhost/linux-ai-input.html", true),
            ("http://tauri.localhost/linux-ai-input.html", true),
            ("http://tauri.localhost:8000/linux-ai-input.html", false),
            ("https://example.test/linux-ai-input.html", false),
            ("tauri://localhost/index.html", false),
            ("tauri://user@localhost/linux-ai-input.html", false),
        ] {
            assert_eq!(trusted_url(&url::Url::parse(value).unwrap()), allowed);
        }
    }
    #[test]
    fn stale_endpoint_replay_cannot_revive_a_replaced_worker() {
        disconnect();
        endpoint(2, Some("/new.sock".into()));
        let epoch = state().lock().unwrap().epoch;
        endpoint(1, Some("/old.sock".into()));
        assert_eq!(
            state().lock().unwrap().endpoint,
            Some(PathBuf::from("/new.sock"))
        );
        assert_eq!(state().lock().unwrap().epoch, epoch);
        endpoint(3, None);
        assert!(state().lock().unwrap().epoch > epoch);
        disconnect();
        endpoint(1, Some("/reconnected.sock".into()));
        assert_eq!(
            state().lock().unwrap().endpoint,
            Some(PathBuf::from("/reconnected.sock"))
        );
        disconnect();
    }
}
