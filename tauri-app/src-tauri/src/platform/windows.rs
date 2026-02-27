use windows::Win32::UI::Input::KeyboardAndMouse::BlockInput;

pub fn block_input(block: bool) -> Result<(), String> {
    let result = unsafe { BlockInput(block) };
    if let Err(err) = result {
        // If block input failed, it's not a critical error, just log it and return Ok
        // TODO notify user
        log::warn!("Failed to block input: {}", err);
        return Ok(());
    }
    Ok(())
}
