//! Resolve the Home directory from the interactive worker's native user
//! identity. Model input and inherited service environment variables are not
//! accepted as path authority.

use std::path::Path;

fn bounded_absolute(path: &Path) -> Option<String> {
    let value = path.to_str()?;
    (path.is_absolute()
        && path.is_dir()
        && !value.is_empty()
        && value.len() <= 4096
        && !value.chars().any(char::is_control))
    .then(|| value.to_owned())
}

#[cfg(windows)]
pub(crate) fn current() -> Option<String> {
    windows::current()
}

#[cfg(unix)]
pub(crate) fn current() -> Option<String> {
    unix::current()
}

#[cfg(not(any(windows, unix)))]
pub(crate) fn current() -> Option<String> {
    None
}

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(test)]
mod tests {
    use super::bounded_absolute;

    #[test]
    fn home_must_be_absolute_and_safe_for_runtime_projection() {
        assert!(bounded_absolute(std::path::Path::new("relative/home")).is_none());
        let current = std::env::current_dir().unwrap();
        assert_eq!(
            bounded_absolute(&current),
            current.to_str().map(ToOwned::to_owned)
        );
        let missing = current.join(format!("missing-home-{}", uuid::Uuid::new_v4()));
        assert!(bounded_absolute(&missing).is_none());
    }
}
