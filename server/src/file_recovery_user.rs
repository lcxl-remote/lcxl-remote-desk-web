//! Native OS identity for recovery scopes. Never derive this from IPC payloads.
#[cfg(target_os = "linux")]
pub(crate) fn current() -> std::io::Result<String> {
    let uid = unsafe { libc::geteuid() };
    if uid == 0 || uid != unsafe { libc::getuid() } {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Linux recovery requires an unelevated user",
        ));
    }
    Ok(uid.to_string())
}
#[cfg(target_os = "macos")]
pub(crate) fn current() -> std::io::Result<String> {
    Ok(unsafe { libc::geteuid() }.to_string())
}
#[cfg(windows)]
pub(crate) fn current() -> std::io::Result<String> {
    let user = desk_file_recovery::windows::current_user_sid()?;
    if matches!(user.as_str(), "S-1-5-18" | "S-1-5-19" | "S-1-5-20") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "system accounts cannot use an interactive recovery namespace",
        ));
    }
    Ok(user)
}
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(crate) fn current() -> std::io::Result<String> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "recovery OS identity unavailable",
    ))
}
