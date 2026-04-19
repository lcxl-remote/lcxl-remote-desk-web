use std::{env, mem::forget, sync::LazyLock, time::Instant};

use serde::Deserialize;
use xcb::{ConnResult, Connection as XcbConnection};
use zbus::Result as ZBusResult;
use zbus::blocking::proxy::SignalIterator;
use zbus::blocking::{Connection as ZBusConnection, Proxy};
use zbus::zvariant::Type;

use crate::error::CaptureError;

pub static XCB_CONNECTION_AND_INDEX: LazyLock<ConnResult<(XcbConnection, i32)>> =
    LazyLock::new(|| {
        let display_name = env::var("DISPLAY").unwrap_or("DISPLAY:1".to_string());
        XcbConnection::connect(Some(display_name.as_str()))
    });

pub static ZBUS_CONNECTION: LazyLock<ZBusResult<ZBusConnection>> =
    LazyLock::new(|| ZBusConnection::session());

pub fn get_zbus_connection() -> Result<&'static ZBusConnection, CaptureError> {
    ZBUS_CONNECTION
        .as_ref()
        .map_err(|err| CaptureError::ZbusError(err.clone()))
}

pub fn get_zbus_portal_request(
    conn: &ZBusConnection,
    handle_token: &str,
) -> Result<Proxy<'static>, CaptureError> {
    let unique_identifier = conn
        .unique_name()
        .ok_or(CaptureError::ZbusError(zbus::Error::Failure(
            "Get DBus unique name failed".to_owned(),
        )))?
        .trim_start_matches(':')
        .replace('.', "_");

    let path =
        format!("/org/freedesktop/portal/desktop/request/{unique_identifier}/{handle_token}");

    let request = Proxy::new(
        conn,
        "org.freedesktop.portal.Desktop",
        path,
        "org.freedesktop.portal.Request",
    )?;

    Ok(request)
}

pub fn wait_zbus_response<'a, T>(
    request: &Proxy<'a>,
    mut response: SignalIterator<'_>,
) -> Result<T, CaptureError>
where
    T: for<'de> Deserialize<'de> + Type,
{
    let start_at = Instant::now();
    log::info!(
        "Portal DBus: waiting Response signal, request_path={}",
        request.path()
    );

    let message = response
        .next()
        .ok_or(CaptureError::ZbusError(zbus::Error::Failure(
            "Failed to get portal response signal".to_owned(),
        )))?;

    let body = message.body();
    let (code, body): (u32, T) = body.deserialize()?;
    log::info!(
        "Portal DBus: got Response signal, request_path={}, code={}, elapsed_ms={}",
        request.path(),
        code,
        start_at.elapsed().as_millis()
    );
    // Workaround: dropping SignalIterator may block in some desktop environments.
    // We intentionally skip drop here to avoid stalling the capture/control flow.
    forget(response);

    if code == 0 {
        return Ok(body);
    }

    if code == 1 {
        return Err(CaptureError::ZbusError(zbus::Error::Failure(
            "Z-Bus canceled".to_owned(),
        )));
    }

    Err(CaptureError::ZbusError(zbus::Error::Failure(format!(
        "Response code is {code}"
    ))))
}
