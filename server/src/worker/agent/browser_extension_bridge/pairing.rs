//! One-use OS-user proof. Only a local command can mint it; HTTP can only consume it.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs::File, io, path::Path};

const RECORD: &str = "browser-pairing-local-proof.json";
const LOCK: &str = "browser-pairing-local-proof.lock";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    version: u16,
    os_user: String,
    digest: [u8; 32],
    created_at: i64,
    expires_at: i64,
    consumed: bool,
}

fn lock(root: &Path) -> io::Result<File> {
    let file = super::private_file::open(&root.join(LOCK), true, 4096)?;
    file.try_lock()
        .map_err(|_| io::Error::other("local pairing is busy"))?;
    Ok(file)
}

fn save(root: &Path, proof: &Proof) -> io::Result<()> {
    super::private_file::write(&root.join(RECORD), &serde_json::to_vec(proof)?)
}

/// Returns a short-lived proof for the local owner's browser, never the pairing secret.
pub fn issue_local_proof(root: &Path) -> io::Result<String> {
    let os_user = crate::file_recovery_service::platform_user::current()?;
    let _lock = lock(root)?;
    let token = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
    let created_at = chrono::Utc::now().timestamp();
    save(
        root,
        &Proof {
            version: 2,
            os_user,
            digest: Sha256::digest(token.as_bytes()).into(),
            created_at,
            expires_at: created_at + 300,
            consumed: false,
        },
    )?;
    Ok(token)
}

pub(crate) fn consume(root: &Path, token: &str) -> io::Result<()> {
    if token.len() != 43 {
        return Err(io::Error::other("local pairing proof is invalid"));
    }
    let os_user = crate::file_recovery_service::platform_user::current()?;
    let _lock = lock(root)?;
    let bytes = super::private_file::read(&root.join(RECORD), 4096)?;
    let mut proof: Proof = serde_json::from_slice(&bytes)?;
    let now = chrono::Utc::now().timestamp();
    let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    if proof.version != 2
        || proof.os_user != os_user
        || proof.consumed
        || now < proof.created_at
        || now >= proof.expires_at
        || !super::constant_time_eq(&digest, &proof.digest)
    {
        return Err(io::Error::other(
            "local pairing proof expired, consumed or invalid",
        ));
    }
    proof.consumed = true;
    save(root, &proof)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn consumed_proof_cannot_be_replayed() {
        let root = crate::worker::agent::browser_extension_bridge::private_test_directory();
        // A manually seeded proof exercises consumption without weakening the
        // production prohibition on elevated Linux users.
        let Ok(os_user) = crate::file_recovery_service::platform_user::current() else {
            assert!(issue_local_proof(root.path()).is_err());
            return;
        };
        let token = URL_SAFE_NO_PAD.encode([7u8; 32]);
        let now = chrono::Utc::now().timestamp();
        save(
            root.path(),
            &Proof {
                version: 2,
                os_user,
                digest: Sha256::digest(token.as_bytes()).into(),
                created_at: now,
                expires_at: now + 60,
                consumed: false,
            },
        )
        .unwrap();
        assert!(consume(root.path(), &URL_SAFE_NO_PAD.encode([8u8; 32])).is_err());
        consume(root.path(), &token).unwrap();
        assert!(consume(root.path(), &token).is_err());
    }
}
