//! Routing of external-origin navigations to the OS default browser.
//!
//! The main webview loads the desk-server frontend from an external HTTP
//! origin. A plain `window.open(url, "_blank")` to another origin (for example
//! the manager console) is swallowed by the webview and never reaches the
//! system browser, so the frontend falls back to a top-level navigation. This
//! module intercepts such navigations: anything leaving the app's own frontend
//! origin is handed to the OS browser and cancelled in-webview, while
//! same-origin navigations proceed normally.

use tauri::{AppHandle, Runtime, Url};
use tauri_plugin_opener::OpenerExt;

/// Returns true when `target` leaves the app's own `frontend_origin`
/// (scheme + host + port) over http(s) — an external link that should open in
/// the OS browser rather than inside the webview. Non-http(s) schemes
/// (`about:`, `data:`, `blob:`, `tauri:`) are never treated as external so the
/// webview keeps handling them.
pub(crate) fn is_external_navigation(target: &Url, frontend_origin: &Url) -> bool {
    if !matches!(target.scheme(), "http" | "https") {
        return false;
    }
    (
        target.scheme(),
        target.host_str(),
        target.port_or_known_default(),
    ) != (
        frontend_origin.scheme(),
        frontend_origin.host_str(),
        frontend_origin.port_or_known_default(),
    )
}

/// Builds the `on_navigation` handler for a main webview: external-origin
/// navigations open in the OS default browser and are cancelled in-webview;
/// same-origin navigations proceed. `frontend_origin` only needs the app's own
/// URL — the comparison ignores path and query.
pub(crate) fn external_link_navigation_handler<R: Runtime>(
    app_handle: AppHandle<R>,
    frontend_origin: Url,
) -> impl Fn(&Url) -> bool + Send + 'static {
    move |url| {
        if is_external_navigation(url, &frontend_origin) {
            if let Err(e) = app_handle.opener().open_url(url.as_str(), None::<&str>) {
                log::error!("Failed to open external URL in system browser: {e}");
            }
            // Never let an external origin load inside the app webview.
            false
        } else {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> Url {
        Url::parse("http://127.0.0.1:8082").unwrap()
    }

    #[test]
    fn external_https_manager_is_external() {
        assert!(is_external_navigation(
            &Url::parse("https://lcxbox.app").unwrap(),
            &origin(),
        ));
    }

    #[test]
    fn same_origin_with_path_and_query_is_internal() {
        assert!(!is_external_navigation(
            &Url::parse("http://127.0.0.1:8082/init?tauri=1").unwrap(),
            &origin(),
        ));
    }

    #[test]
    fn different_port_is_external() {
        assert!(is_external_navigation(
            &Url::parse("http://127.0.0.1:5174/").unwrap(),
            &origin(),
        ));
    }

    #[test]
    fn different_host_is_external() {
        assert!(is_external_navigation(
            &Url::parse("http://example.com:8082/").unwrap(),
            &origin(),
        ));
    }

    #[test]
    fn scheme_upgrade_same_host_is_external() {
        // http frontend vs https same host:port differs in scheme → external.
        assert!(is_external_navigation(
            &Url::parse("https://127.0.0.1:8082/").unwrap(),
            &origin(),
        ));
    }

    #[test]
    fn non_http_scheme_is_not_external() {
        // about:blank / data: URLs must not be hijacked to the OS browser.
        assert!(!is_external_navigation(
            &Url::parse("about:blank").unwrap(),
            &origin(),
        ));
        assert!(!is_external_navigation(
            &Url::parse("data:text/html,hi").unwrap(),
            &origin(),
        ));
    }
}
