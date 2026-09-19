use std::os::unix::process::ExitStatusExt;
pub(super) fn signal(status: &std::process::ExitStatus) -> Option<i32> {
    status.signal()
}
