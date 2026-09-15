use super::*;
use crate::windows::{FileKind, file_identity};
use ::windows::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
};
use std::{fs, os::windows::fs::OpenOptionsExt};

fn directory(path: &Path) -> File {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
        .open(path)
        .unwrap()
}
fn scope() -> Scope {
    Scope {
        authority: "central".into(),
        device: "device".into(),
        os_user: "user".into(),
        owner: "owner".into(),
    }
}
fn backup(vault: &mut LockedVault) -> Record {
    vault
        .backup(BackupRequest {
            scope: scope(),
            conversation: "conversation",
            operation: "operation",
            generation: "generation",
            file_name: "source.txt",
            content: b"before",
            metadata: b"{}",
            now_ms: 1000,
        })
        .unwrap()
}

#[test]
fn windows_transaction_identity_is_durable_and_monotonic() {
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::open(root.path()).unwrap();
    let mut locked = vault.lock().unwrap();
    let record = backup(&mut locked);
    let source = root.path().join("source.txt");
    fs::write(&source, b"before").unwrap();
    let parent = directory(root.path());
    let original = File::open(&source).unwrap();
    let name = locked
        .plan_windows_transaction(&scope(), &record.id, root.path(), &parent, &original)
        .unwrap();
    assert!(!root.path().join(&name).exists());
    let planned = locked.ledger.records[&record.id]
        .transaction
        .clone()
        .unwrap();
    drop(locked);
    let mut locked = vault.lock().unwrap();
    assert_eq!(
        serde_json::to_vec(&planned).unwrap(),
        serde_json::to_vec(
            &locked.ledger.records[&record.id]
                .transaction
                .as_ref()
                .unwrap()
        )
        .unwrap()
    );
    assert!(
        locked
            .plan_windows_transaction(&scope(), &record.id, root.path(), &parent, &original)
            .is_err()
    );
    let _private_transaction = locked
        .create_windows_transaction_directory(&scope(), &record.id, &parent)
        .unwrap();
    assert!(
        locked
            .create_windows_transaction_directory(&scope(), &record.id, &parent)
            .is_err()
    );
    let transaction = directory(&root.path().join(&name));
    locked
        .register_windows_transaction(&scope(), &record.id, &parent, &transaction, None)
        .unwrap();
    fs::write(root.path().join(&name).join("replacement"), b"after").unwrap();
    let staged = File::open(root.path().join(&name).join("replacement")).unwrap();
    locked
        .register_windows_transaction(&scope(), &record.id, &parent, &transaction, Some(&staged))
        .unwrap();
    let saved = serde_json::to_vec(&locked.ledger.records[&record.id].transaction).unwrap();
    assert!(
        locked
            .register_windows_transaction(&scope(), &record.id, &parent, &transaction, None)
            .is_err()
    );
    assert!(
        locked
            .register_windows_transaction(
                &scope(),
                &record.id,
                &parent,
                &transaction,
                Some(&original)
            )
            .is_err()
    );
    let mut other_scope = scope();
    other_scope.owner = "another owner".into();
    assert!(
        locked
            .register_windows_transaction(
                &other_scope,
                &record.id,
                &parent,
                &transaction,
                Some(&staged)
            )
            .is_err()
    );
    assert_eq!(
        serde_json::to_vec(&locked.ledger.records[&record.id].transaction).unwrap(),
        saved
    );
    drop(locked);
    let mut locked = vault.lock().unwrap();
    let TransactionIdentity::Windows { files } = &locked.ledger.records[&record.id]
        .transaction
        .as_ref()
        .unwrap()
        .identity
    else {
        panic!("wrong platform");
    };
    assert_eq!(
        files.original_file_id,
        file_identity(&original, FileKind::File).unwrap().file_id
    );
    assert_eq!(
        files.staged_file_id,
        Some(file_identity(&staged, FileKind::File).unwrap().file_id)
    );
    assert_eq!(
        files.directory_file_id,
        Some(
            file_identity(&transaction, FileKind::Directory)
                .unwrap()
                .file_id
        )
    );
    locked
        .transition(&scope(), &record.id, ChangeState::CommitIntent)
        .unwrap();
    assert!(
        locked
            .register_windows_transaction(
                &scope(),
                &record.id,
                &parent,
                &transaction,
                Some(&staged)
            )
            .is_err()
    );
    drop(locked);
    let mut locked = vault.lock().unwrap();
    let unknown = locked.recover_interrupted(1001).unwrap();
    assert_eq!(unknown.len(), 1);
    let recovered = &locked.ledger.records[&record.id];
    assert_eq!(recovered.change, ChangeState::OutcomeUnknown);
    assert_eq!(recovered.material, MaterialState::Saved);
    assert!(recovered.transaction.is_some());
    assert!(recovered.cleanup_error.is_some());
    assert_eq!(fs::read(&source).unwrap(), b"before");
    assert_eq!(
        fs::read(root.path().join(&name).join("replacement")).unwrap(),
        b"after"
    );
}

#[test]
fn failed_directory_registration_retains_material_and_refuses_adoption_after_reopen() {
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::open(root.path()).unwrap();
    let mut locked = vault.lock().unwrap();
    let record = backup(&mut locked);
    let source = root.path().join("source.txt");
    fs::write(&source, b"before").unwrap();
    let parent = directory(root.path());
    let original = File::open(&source).unwrap();
    let name = locked
        .plan_windows_transaction(&scope(), &record.id, root.path(), &parent, &original)
        .unwrap();
    let mut other = scope();
    other.owner = "other".into();
    assert!(
        locked
            .create_windows_transaction_directory(&other, &record.id, &parent)
            .is_err()
    );
    assert!(!root.path().join(&name).exists());
    let reader = fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(root.path().join("file-recovery/index.json"))
        .unwrap();
    let error = locked
        .create_windows_transaction_directory(&scope(), &record.id, &parent)
        .err()
        .unwrap();
    assert_eq!(
        error
            .get_ref()
            .unwrap()
            .downcast_ref::<crate::windows::IndexWriteError>()
            .unwrap()
            .outcome,
        crate::windows::IndexWriteOutcome::OutcomeUnknown
    );
    assert!(root.path().join(&name).is_dir());
    assert!(
        locked
            .create_windows_transaction_directory(&scope(), &record.id, &parent)
            .is_err()
    );
    drop((reader, locked));
    let mut reopened = vault.lock().unwrap();
    let TransactionIdentity::Windows { files } = &reopened.ledger.records[&record.id]
        .transaction
        .as_ref()
        .unwrap()
        .identity
    else {
        panic!("wrong platform");
    };
    assert!(files.directory_file_id.is_none());
    assert_eq!(
        reopened
            .create_windows_transaction_directory(&scope(), &record.id, &parent)
            .err()
            .unwrap()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert!(root.path().join(&name).is_dir());
    assert_eq!(fs::read(source).unwrap(), b"before");
    assert_eq!(
        reopened.export(&scope(), &record.id, 1001).unwrap().1,
        b"before"
    );
}

#[test]
fn windows_registration_rejects_nonprivate_transaction_directory_without_repairing_it() {
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::open(root.path()).unwrap();
    let mut locked = vault.lock().unwrap();
    let record = backup(&mut locked);
    let source = root.path().join("source.txt");
    fs::write(&source, b"before").unwrap();
    let parent = directory(root.path());
    let original = File::open(&source).unwrap();
    let name = locked
        .plan_windows_transaction(&scope(), &record.id, root.path(), &parent, &original)
        .unwrap();
    let path = root.path().join(&name);
    fs::create_dir(&path).unwrap();
    fs::write(path.join("sentinel"), b"unrelated").unwrap();
    let transaction = directory(&path);
    let before = fs::read(root.path().join("file-recovery/index.json")).unwrap();
    assert!(
        locked
            .register_windows_transaction(&scope(), &record.id, &parent, &transaction, None)
            .is_err()
    );
    assert!(crate::windows::PrivateDirectory::create_new(&parent, &name).is_err());
    assert!(crate::windows::PrivateDirectory::open_or_create(&parent, &name).is_err());
    assert_eq!(
        fs::read(root.path().join("file-recovery/index.json")).unwrap(),
        before
    );
    assert_eq!(fs::read(path.join("sentinel")).unwrap(), b"unrelated");
}

#[test]
fn windows_registration_rejects_objects_outside_the_recorded_location() {
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::open(root.path()).unwrap();
    let mut locked = vault.lock().unwrap();
    let record = backup(&mut locked);
    let source = root.path().join("source.txt");
    fs::write(&source, b"before").unwrap();
    let parent = directory(root.path());
    let original = File::open(&source).unwrap();
    let other_path = root.path().join("other");
    fs::create_dir(&other_path).unwrap();
    assert!(
        locked
            .plan_windows_transaction(&scope(), &record.id, &other_path, &parent, &original)
            .is_err()
    );
    assert!(locked.ledger.records[&record.id].transaction.is_none());
    let name = locked
        .plan_windows_transaction(&scope(), &record.id, root.path(), &parent, &original)
        .unwrap();
    let transaction_path = root.path().join(&name);
    let private_transaction = crate::windows::PrivateDirectory::create_new(&parent, &name).unwrap();
    let transaction = directory(&transaction_path);
    let other = directory(&other_path);
    let index_path = root.path().join("file-recovery/index.json");
    let planned = fs::read(&index_path).unwrap();
    assert!(
        locked
            .register_windows_transaction(&scope(), &record.id, &parent, &other, None)
            .is_err()
    );
    // Use the production creation guard: it owns DELETE access but denies
    // delete sharing to other handles for the entire registration/commit.
    let mut replacement = private_transaction.create_replacement().unwrap();
    std::io::Write::write_all(&mut replacement, b"replacement").unwrap();
    fs::write(other_path.join("replacement"), b"unrelated").unwrap();
    let unrelated = File::open(other_path.join("replacement")).unwrap();
    assert!(
        locked
            .register_windows_transaction(
                &scope(),
                &record.id,
                &parent,
                &transaction,
                Some(&unrelated)
            )
            .is_err()
    );
    assert_eq!(fs::read(&index_path).unwrap(), planned);
    let held = crate::windows::transaction_location(
        root.path(),
        &parent,
        &name,
        Some(&transaction),
        Some(&replacement),
    )
    .unwrap();
    assert!(
        fs::rename(
            transaction_path.join("replacement"),
            transaction_path.join("moved")
        )
        .is_err()
    );
    assert!(fs::rename(&transaction_path, root.path().join("moved-directory")).is_err());
    drop(held);
    locked
        .register_windows_transaction(
            &scope(),
            &record.id,
            &parent,
            &transaction,
            Some(&replacement),
        )
        .unwrap();
    assert_eq!(
        fs::read(other_path.join("replacement")).unwrap(),
        b"unrelated"
    );
}

#[test]
fn failed_windows_plan_publication_blocks_commit_until_reopen() {
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::open(root.path()).unwrap();
    let mut locked = vault.lock().unwrap();
    let record = backup(&mut locked);
    let source = root.path().join("source.txt");
    fs::write(&source, b"before").unwrap();
    let parent = directory(root.path());
    let original = File::open(&source).unwrap();
    let reader = fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(root.path().join("file-recovery/index.json"))
        .unwrap();
    let error = locked
        .plan_windows_transaction(&scope(), &record.id, root.path(), &parent, &original)
        .unwrap_err();
    let publication = error
        .get_ref()
        .unwrap()
        .downcast_ref::<crate::windows::IndexWriteError>()
        .unwrap();
    assert_eq!(
        publication.outcome,
        crate::windows::IndexWriteOutcome::OutcomeUnknown
    );
    assert!(
        locked
            .create_windows_transaction_directory(&scope(), &record.id, &parent)
            .is_err()
    );
    assert!(
        locked
            .transition(&scope(), &record.id, ChangeState::CommitIntent)
            .is_err()
    );
    assert!(
        !root
            .path()
            .join(format!(".assistant-transaction-{}", record.id))
            .exists()
    );
    assert_eq!(fs::read(&source).unwrap(), b"before");
    drop((reader, locked));
    let reopened = vault.lock().unwrap();
    let actual = &reopened.ledger.records[&record.id];
    assert!(actual.transaction.is_none());
    assert_eq!(actual.change, ChangeState::BackupReady);
    assert!(reopened.storage.extra_used_bytes().unwrap() > 0);
    assert_eq!(
        reopened.export(&scope(), &record.id, 1001).unwrap().1,
        b"before"
    );
}

#[test]
fn windows_plan_rejects_wrong_scope_and_unbacked_record_without_creating_files() {
    let root = tempfile::tempdir().unwrap();
    let vault = Vault::open(root.path()).unwrap();
    let mut locked = vault.lock().unwrap();
    let record = backup(&mut locked);
    let source = root.path().join("source.txt");
    fs::write(&source, b"before").unwrap();
    let parent = directory(root.path());
    let original = File::open(&source).unwrap();
    let mut other = scope();
    other.device = "other device".into();
    assert!(
        locked
            .plan_windows_transaction(&other, &record.id, root.path(), &parent, &original)
            .is_err()
    );
    assert!(
        locked
            .plan_windows_transaction(
                &scope(),
                &record.id,
                Path::new("relative"),
                &parent,
                &original
            )
            .is_err()
    );
    assert!(locked.ledger.records[&record.id].transaction.is_none());
    locked
        .transition(&scope(), &record.id, ChangeState::Aborted)
        .unwrap();
    assert!(
        locked
            .plan_windows_transaction(&scope(), &record.id, root.path(), &parent, &original)
            .is_err()
    );
    assert!(locked.ledger.records[&record.id].transaction.is_none());
    assert_eq!(fs::read(&source).unwrap(), b"before");
}
