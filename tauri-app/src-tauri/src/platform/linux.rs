use std::sync::{Mutex, OnceLock};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt, GrabMode};
use x11rb::rust_connection::RustConnection;

struct X11Grabber {
    conn: RustConnection,
    root: u32,
}

static X11_GRABBER: OnceLock<Mutex<Option<X11Grabber>>> = OnceLock::new();

fn grabber_slot() -> &'static Mutex<Option<X11Grabber>> {
    X11_GRABBER.get_or_init(|| Mutex::new(None))
}

pub fn block_input(block: bool) -> Result<(), String> {
    // Wayland: not supported
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        log::warn!("Input blocking not supported on Wayland");
        return Ok(());
    }

    if block {
        let mut guard = grabber_slot()
            .lock()
            .map_err(|e| format!("Failed to acquire X11 grabber lock: {}", e))?;
        if guard.is_some() {
            return Ok(()); // Already grabbed
        }

        let (conn, screen_num) = RustConnection::connect(None)
            .map_err(|e| format!("Failed to connect to X11: {}", e))?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;

        // Grab keyboard: owner_events=false, async mode, current time
        let kb_reply = conn
            .grab_keyboard(
                false,
                root,
                xproto::CURRENT_TIME,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .map_err(|e| format!("grab_keyboard request failed: {}", e))?
            .reply()
            .map_err(|e| format!("grab_keyboard reply failed: {}", e))?;

        if kb_reply.status != xproto::GrabStatus::SUCCESS {
            return Err(format!(
                "grab_keyboard failed with status: {:?}",
                kb_reply.status
            ));
        }

        // Grab pointer: owner_events=false, no event mask, no confine, no cursor change
        let ptr_reply = conn
            .grab_pointer(
                false,
                root,
                0u16, // empty event mask — swallow all pointer events
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                x11rb::NONE, // confine_to: none
                x11rb::NONE, // cursor: none
                xproto::CURRENT_TIME,
            )
            .map_err(|e| format!("grab_pointer request failed: {}", e))?
            .reply()
            .map_err(|e| format!("grab_pointer reply failed: {}", e))?;

        if ptr_reply.status != xproto::GrabStatus::SUCCESS {
            // Keyboard was already grabbed, release it before returning error
            let _ = conn.ungrab_keyboard(xproto::CURRENT_TIME);
            let _ = conn.flush();
            return Err(format!(
                "grab_pointer failed with status: {:?}",
                ptr_reply.status
            ));
        }

        conn.flush()
            .map_err(|e| format!("Failed to flush X11 connection: {}", e))?;

        log::info!("Linux X11: keyboard and pointer grabbed");
        *guard = Some(X11Grabber { conn, root: root });
    } else {
        let mut guard = grabber_slot()
            .lock()
            .map_err(|e| format!("Failed to acquire X11 grabber lock: {}", e))?;
        if let Some(grabber) = guard.take() {
            let _ = grabber.conn.ungrab_keyboard(xproto::CURRENT_TIME);
            let _ = grabber.conn.ungrab_pointer(xproto::CURRENT_TIME);
            let _ = grabber.conn.flush();
            log::info!("Linux X11: keyboard and pointer ungrabbed");
        }
    }

    Ok(())
}
