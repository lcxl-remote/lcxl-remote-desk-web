use std::fmt;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use desk_utils::durable_file::{FileMode, durable_atomic_write};
use serde::{Deserialize, Serialize};

use crate::{AuthorizationTarget, PortalError};

const TOKEN_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, PartialEq, Eq)]
pub struct RestoreToken {
    pub target: AuthorizationTarget,
    pub token: String,
}

impl fmt::Debug for RestoreToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RestoreToken")
            .field("target", &self.target)
            .field("token", &"<redacted>")
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum TokenRecord {
    Available {
        version: u32,
        app_id: String,
        desktop_fingerprint: String,
        target: AuthorizationTarget,
        token: String,
    },
    Consumed {
        version: u32,
        app_id: String,
        desktop_fingerprint: String,
    },
}

#[derive(Debug, Clone)]
pub struct RestoreTokenStore {
    path: PathBuf,
    lock_path: PathBuf,
    app_id: String,
    desktop_fingerprint: String,
}

impl RestoreTokenStore {
    #[cfg(target_os = "linux")]
    pub fn for_current_user(app_id: &str) -> Option<Self> {
        let state_root = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .map(|home| home.join(".local/state"))
            })?;
        let desktop_fingerprint = std::env::var("XDG_CURRENT_DESKTOP")
            .ok()
            .or_else(|| std::env::var("XDG_SESSION_DESKTOP").ok())
            .map(|value| normalize_desktop_fingerprint(&value))
            .filter(|value| !value.is_empty())?;
        Some(Self::new(
            state_root
                .join("lcxl-remote-desk")
                .join("wayland-portal.json"),
            app_id,
            &desktop_fingerprint,
        ))
    }

    pub fn new(path: PathBuf, app_id: &str, desktop_fingerprint: &str) -> Self {
        let lock_path = path.with_extension("lock");
        Self {
            path,
            lock_path,
            app_id: app_id.to_owned(),
            desktop_fingerprint: normalize_desktop_fingerprint(desktop_fingerprint),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn consume(&self) -> Result<Option<RestoreToken>, PortalError> {
        self.with_lock(|| {
            let bytes = match fs::read(&self.path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            let record: TokenRecord = serde_json::from_slice(&bytes)?;
            match record {
                TokenRecord::Consumed {
                    version,
                    app_id,
                    desktop_fingerprint,
                } => {
                    self.validate_identity(version, &app_id, &desktop_fingerprint)?;
                    Ok(None)
                }
                TokenRecord::Available {
                    version,
                    app_id,
                    desktop_fingerprint,
                    target,
                    token,
                } => {
                    self.validate_identity(version, &app_id, &desktop_fingerprint)?;
                    if token.is_empty() {
                        return Err(PortalError::InvalidTokenStore);
                    }
                    let consumed = TokenRecord::Consumed {
                        version: TOKEN_SCHEMA_VERSION,
                        app_id: self.app_id.clone(),
                        desktop_fingerprint: self.desktop_fingerprint.clone(),
                    };
                    durable_atomic_write(
                        &self.path,
                        &serde_json::to_vec(&consumed)?,
                        FileMode::OwnerOnly,
                    )?;
                    Ok(Some(RestoreToken { target, token }))
                }
            }
        })
    }

    pub fn rotate(&self, token: Option<&RestoreToken>) -> Result<(), PortalError> {
        self.with_lock(|| match token {
            Some(token) => {
                if token.token.is_empty() {
                    return Err(PortalError::InvalidTokenStore);
                }
                let record = TokenRecord::Available {
                    version: TOKEN_SCHEMA_VERSION,
                    app_id: self.app_id.clone(),
                    desktop_fingerprint: self.desktop_fingerprint.clone(),
                    target: token.target,
                    token: token.token.clone(),
                };
                let json = serde_json::to_vec(&record)?;
                durable_atomic_write(&self.path, &json, FileMode::OwnerOnly)?;
                Ok(())
            }
            None => match fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            },
        })
    }

    fn validate_identity(
        &self,
        version: u32,
        app_id: &str,
        desktop_fingerprint: &str,
    ) -> Result<(), PortalError> {
        if version != TOKEN_SCHEMA_VERSION
            || app_id != self.app_id
            || desktop_fingerprint != self.desktop_fingerprint
        {
            return Err(PortalError::InvalidTokenStore);
        }
        Ok(())
    }

    fn with_lock<T>(
        &self,
        operation: impl FnOnce() -> Result<T, PortalError>,
    ) -> Result<T, PortalError> {
        if let Some(parent) = self.lock_path.parent() {
            fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let lock = options.open(&self.lock_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            lock.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        lock.lock()?;
        operation()
    }
}

fn normalize_desktop_fingerprint(value: &str) -> String {
    value
        .split(':')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_consumed_once_and_rotated() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = RestoreTokenStore::new(
            directory.path().join("wayland-portal.json"),
            "com.lcxl.remote-desk",
            "GNOME:GNOME",
        );
        let token = RestoreToken {
            target: AuthorizationTarget::ScreenAndInput,
            token: "first".into(),
        };

        store.rotate(Some(&token)).expect("store token");
        assert_eq!(store.consume().expect("consume"), Some(token));
        assert_eq!(store.consume().expect("consume again"), None);

        let rotated = RestoreToken {
            target: AuthorizationTarget::ScreenOnly,
            token: "second".into(),
        };
        store.rotate(Some(&rotated)).expect("rotate token");
        assert_eq!(store.consume().expect("consume rotated"), Some(rotated));
    }

    #[test]
    fn corrupt_or_empty_token_fails_closed() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("wayland-portal.json");
        let store = RestoreTokenStore::new(path.clone(), "com.lcxl.remote-desk", "gnome");
        fs::write(&path, b"not-json").expect("write corrupt token");
        assert!(matches!(store.consume(), Err(PortalError::Json(_))));

        fs::write(&path, br#"{"state":"available","version":1,"app_id":"com.lcxl.remote-desk","desktop_fingerprint":"gnome","target":"screen_only","token":""}"#)
            .expect("write empty token");
        assert!(matches!(
            store.consume(),
            Err(PortalError::InvalidTokenStore)
        ));
    }

    #[test]
    fn identity_mismatch_is_rejected_without_exposing_token() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("wayland-portal.json");
        let gnome = RestoreTokenStore::new(path.clone(), "com.lcxl.remote-desk", "gnome");
        gnome
            .rotate(Some(&RestoreToken {
                target: AuthorizationTarget::ScreenAndInput,
                token: "secret-token".into(),
            }))
            .expect("store token");
        let kde = RestoreTokenStore::new(path, "com.lcxl.remote-desk", "kde");
        assert!(matches!(kde.consume(), Err(PortalError::InvalidTokenStore)));
        assert!(
            !format!(
                "{:?}",
                RestoreToken {
                    target: AuthorizationTarget::ScreenOnly,
                    token: "secret-token".into(),
                }
            )
            .contains("secret-token")
        );
    }

    #[test]
    fn concurrent_consumers_observe_token_at_most_once() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = RestoreTokenStore::new(
            directory.path().join("wayland-portal.json"),
            "com.lcxl.remote-desk",
            "gnome",
        );
        store
            .rotate(Some(&RestoreToken {
                target: AuthorizationTarget::ScreenAndInput,
                token: "single-use".into(),
            }))
            .expect("store token");

        let first = store.clone();
        let second = store.clone();
        let first = std::thread::spawn(move || first.consume().expect("first consume"));
        let second = std::thread::spawn(move || second.consume().expect("second consume"));
        let results = [first.join().unwrap(), second.join().unwrap()];
        assert_eq!(results.iter().filter(|token| token.is_some()).count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn token_directory_and_lock_file_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("tempdir");
        let state = directory.path().join("state");
        let store = RestoreTokenStore::new(
            state.join("wayland-portal.json"),
            "com.lcxl.remote-desk",
            "gnome",
        );
        store.consume().expect("empty consume");
        assert_eq!(
            fs::metadata(&state).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(state.join("wayland-portal.lock"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
