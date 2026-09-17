//! Append-only launch receipts survive worker replacement without retry leases.
//!
//! A single exclusive claim file arbitrates cancellation against native dispatch.
//! A torn claim is treated as unknown and never permits another native invocation.
use desk_agent_protocol::application_launch::LaunchApplicationResult;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{self, Read, Write},
    path::PathBuf,
};

#[cfg(unix)]
#[path = "journal/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "journal/windows.rs"]
mod platform;

const MAX_RECORD_BYTES: u64 = 128 * 1024;

pub(crate) struct LaunchJournal {
    root: PathBuf,
    storage: platform::Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InvocationClaim {
    Invoke,
    Cancelled,
    Recorded(LaunchApplicationResult),
    OutcomeUnknown,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    dispatch_id: String,
    digest: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Claim {
    Invoking,
    Cancelled,
}

impl LaunchJournal {
    /// `root` is private application storage, never a model-supplied directory.
    pub(crate) fn open(root: PathBuf) -> io::Result<Self> {
        let storage = platform::Directory::root(&root)?;
        Ok(Self { root, storage })
    }

    fn directory(&self, dispatch_id: &str) -> io::Result<PathBuf> {
        if dispatch_id.is_empty() || dispatch_id.len() > 512 {
            return Err(io::Error::other("invalid launch dispatch id"));
        }
        Ok(self
            .root
            .join(format!("{:x}", Sha256::digest(dispatch_id.as_bytes()))))
    }

    /// Preparation is idempotent only for the same frozen authority digest.
    pub(crate) fn prepare(&self, dispatch_id: &str, digest: &str) -> io::Result<()> {
        if digest.len() != 64 || !digest.bytes().all(|v| v.is_ascii_hexdigit()) {
            return Err(io::Error::other("invalid launch authority digest"));
        }
        let directory = self.directory(dispatch_id)?;
        let storage = self.storage.child(directory.file_name().unwrap(), true)?;
        let binding = Binding {
            dispatch_id: dispatch_id.into(),
            digest: digest.into(),
        };
        match write_exclusive(&storage, "binding.json", &binding) {
            Ok(()) => storage.sync(),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                self.verify_binding(&storage, dispatch_id, digest)
            }
            Err(e) => Err(e),
        }
    }

    fn verify_binding(
        &self,
        storage: &platform::Directory,
        dispatch_id: &str,
        digest: &str,
    ) -> io::Result<()> {
        let binding: Binding = serde_json::from_slice(&read_record(storage, "binding.json")?)?;
        if binding.dispatch_id != dispatch_id || binding.digest != digest {
            return Err(io::Error::other("launch dispatch authority mismatch"));
        }
        Ok(())
    }

    pub(crate) fn claim(&self, dispatch_id: &str, digest: &str) -> io::Result<InvocationClaim> {
        let directory = self.directory(dispatch_id)?;
        let storage = self.storage.child(directory.file_name().unwrap(), false)?;
        self.verify_binding(&storage, dispatch_id, digest)?;
        match write_exclusive(&storage, "claim.json", &Claim::Invoking) {
            Ok(()) => {
                // Do not call the OS until the durable invocation boundary is committed.
                storage.sync()?;
                Ok(InvocationClaim::Invoke)
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                self.read_existing(&storage, dispatch_id)
            }
            Err(e) => Err(e),
        }
    }

    pub(crate) fn existing(
        &self,
        dispatch_id: &str,
        digest: &str,
    ) -> io::Result<Option<InvocationClaim>> {
        let directory = self.directory(dispatch_id)?;
        let storage = self.storage.child(directory.file_name().unwrap(), false)?;
        self.verify_binding(&storage, dispatch_id, digest)?;
        match storage.file("claim.json", false) {
            Ok(file) => {
                drop(file);
                self.read_existing(&storage, dispatch_id).map(Some)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn read_existing(
        &self,
        storage: &platform::Directory,
        dispatch_id: &str,
    ) -> io::Result<InvocationClaim> {
        let claim = read_record(storage, "claim.json")?;
        match serde_json::from_slice::<Claim>(&claim) {
            Ok(Claim::Cancelled) => Ok(InvocationClaim::Cancelled),
            Ok(Claim::Invoking) => match read_record(storage, "result.json") {
                Ok(bytes) => Ok(
                    match serde_json::from_slice::<LaunchApplicationResult>(&bytes) {
                        Ok(result) if result.dispatch_id == dispatch_id => {
                            InvocationClaim::Recorded(result)
                        }
                        _ => InvocationClaim::OutcomeUnknown,
                    },
                ),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    Ok(InvocationClaim::OutcomeUnknown)
                }
                Err(e) => Err(e),
            },
            Err(_) => Ok(InvocationClaim::OutcomeUnknown),
        }
    }

    /// Cancellation and invocation compete for the very same exclusive file.
    pub(crate) fn cancel(&self, dispatch_id: &str, digest: &str) -> io::Result<InvocationClaim> {
        let directory = self.directory(dispatch_id)?;
        let storage = self.storage.child(directory.file_name().unwrap(), false)?;
        self.verify_binding(&storage, dispatch_id, digest)?;
        match write_exclusive(&storage, "claim.json", &Claim::Cancelled) {
            Ok(()) => {
                storage.sync()?;
                Ok(InvocationClaim::Cancelled)
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                self.read_existing(&storage, dispatch_id)
            }
            Err(e) => Err(e),
        }
    }

    pub(crate) fn record(
        &self,
        dispatch_id: &str,
        digest: &str,
        result: &LaunchApplicationResult,
    ) -> io::Result<()> {
        let directory = self.directory(dispatch_id)?;
        let storage = self.storage.child(directory.file_name().unwrap(), false)?;
        self.verify_binding(&storage, dispatch_id, digest)?;
        if result.dispatch_id != dispatch_id
            || !matches!(
                serde_json::from_slice::<Claim>(&read_record(&storage, "claim.json")?)?,
                Claim::Invoking
            )
        {
            return Err(io::Error::other("launch result has no invocation claim"));
        }
        write_exclusive(&storage, "result.json", result)?;
        storage.sync()
    }
}

fn write_exclusive(
    storage: &platform::Directory,
    name: &str,
    value: &impl Serialize,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(io::Error::other("launch journal record exceeds size limit"));
    }
    let mut file = storage.file(name, true)?;
    file.write_all(&bytes)?;
    file.sync_all()
}

fn read_record(storage: &platform::Directory, name: &str) -> io::Result<Vec<u8>> {
    let file = storage.file(name, false)?;
    if file.metadata()?.len() > MAX_RECORD_BYTES {
        return Err(io::Error::other("launch journal record exceeds size limit"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(io::Error::other("launch journal record exceeds size limit"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    fn journal() -> LaunchJournal {
        LaunchJournal::open(
            std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("desk-launch-journal-{}", uuid::Uuid::new_v4())),
        )
        .unwrap()
    }

    #[test]
    fn oversized_receipts_cannot_trigger_unbounded_reads_or_a_second_invocation() {
        let journal = journal();
        let digest = "a".repeat(64);
        journal.prepare("dispatch", &digest).unwrap();
        assert_eq!(
            journal.claim("dispatch", &digest).unwrap(),
            InvocationClaim::Invoke
        );
        let result_path = journal.directory("dispatch").unwrap().join("result.json");
        let file = File::create(&result_path).unwrap();
        file.set_len(MAX_RECORD_BYTES + 1).unwrap();
        drop(file);
        assert!(journal.existing("dispatch", &digest).is_err());
        assert!(journal.claim("dispatch", &digest).is_err());
        fs::remove_file(result_path).unwrap();
        assert_eq!(
            journal.claim("dispatch", &digest).unwrap(),
            InvocationClaim::OutcomeUnknown
        );
        let root = journal.root.clone();
        drop(journal);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn receipt_hard_links_are_rejected() {
        let journal = journal();
        let digest = "a".repeat(64);
        journal.prepare("dispatch", &digest).unwrap();
        let original = journal.directory("dispatch").unwrap().join("binding.json");
        fs::hard_link(&original, journal.root.join("binding-alias.json")).unwrap();
        assert!(journal.claim("dispatch", &digest).is_err());
        assert!(
            !journal
                .directory("dispatch")
                .unwrap()
                .join("claim.json")
                .exists()
        );
        let root = journal.root.clone();
        drop(journal);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn receipt_symlinks_are_rejected() {
        let journal = journal();
        let outside = journal.root.join("outside.json");
        fs::write(&outside, b"{}").unwrap();
        let alias = journal.root.join("alias.json");
        std::os::unix::fs::symlink(&outside, &alias).unwrap();
        assert!(read_record(&journal.storage, "alias.json").is_err());
        let root = journal.root.clone();
        drop(journal);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn directory_replacement_cannot_redirect_receipt_writes() {
        let journal = journal();
        let root = journal.root.clone();
        let moved = root.with_extension("moved");
        fs::rename(&root, &moved).unwrap();
        fs::create_dir(&root).unwrap();
        journal.prepare("dispatch", &"a".repeat(64)).unwrap();
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        assert_eq!(fs::read_dir(&moved).unwrap().count(), 1);
        drop(journal);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(moved).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn held_directory_cannot_be_replaced_during_dispatch() {
        let journal = journal();
        let root = journal.root.clone();
        let moved = root.with_extension("moved");
        assert!(fs::rename(&root, &moved).is_err());
        journal.prepare("dispatch", &"a".repeat(64)).unwrap();
        drop(journal);
        fs::rename(&root, &moved).unwrap();
        fs::remove_dir_all(moved).unwrap();
    }
    #[test]
    fn cancellation_is_durable_and_cannot_be_reversed_by_a_new_worker() {
        let journal = journal();
        let digest = "a".repeat(64);
        journal.prepare("dispatch", &digest).unwrap();
        assert_eq!(
            journal.cancel("dispatch", &digest).unwrap(),
            InvocationClaim::Cancelled
        );
        let reopened = LaunchJournal::open(journal.root.clone()).unwrap();
        assert_eq!(
            reopened.claim("dispatch", &digest).unwrap(),
            InvocationClaim::Cancelled
        );
        drop(reopened);
        let root = journal.root.clone();
        drop(journal);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn invocation_is_never_reissued_after_restart_or_torn_receipt() {
        let journal = journal();
        let digest = "a".repeat(64);
        journal.prepare("dispatch", &digest).unwrap();
        assert_eq!(
            journal.claim("dispatch", &digest).unwrap(),
            InvocationClaim::Invoke
        );
        let reopened = LaunchJournal::open(journal.root.clone()).unwrap();
        assert_eq!(
            reopened.claim("dispatch", &digest).unwrap(),
            InvocationClaim::OutcomeUnknown
        );
        assert_eq!(
            reopened.cancel("dispatch", &digest).unwrap(),
            InvocationClaim::OutcomeUnknown
        );
        fs::write(
            journal.directory("dispatch").unwrap().join("result.json"),
            b"{",
        )
        .unwrap();
        assert_eq!(
            reopened.claim("dispatch", &digest).unwrap(),
            InvocationClaim::OutcomeUnknown
        );
        assert!(journal.prepare("dispatch", &"b".repeat(64)).is_err());
        drop(reopened);
        let root = journal.root.clone();
        drop(journal);
        fs::remove_dir_all(root).unwrap();
    }
}
