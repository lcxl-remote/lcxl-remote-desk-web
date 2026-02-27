pub fn block_input(block: bool) -> Result<(), String> {
    // 检查是否 Wayland
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        return Err("Input blocking not supported on Wayland".to_string());
    }

    // X11: 使用 XGrabKeyboard 和 XGrabPointer
    // 需要通过 x11rb crate 实现
    if block {
        // TODO: x11rb::protocol::xproto::grab_keyboard + grab_pointer
        log::info!("Linux X11: Would grab keyboard and pointer");
    } else {
        // TODO: x11rb::protocol::xproto::ungrab_keyboard + ungrab_pointer
        log::info!("Linux X11: Would ungrab keyboard and pointer");
    }
    Ok(())
}
