use super::*;
use std::{
    fs::{self, OpenOptions},
    os::windows::fs::OpenOptionsExt,
    path::Path,
};

fn directory(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
        .open(path)
        .unwrap()
}

#[test]
fn handle_identity_survives_rename_and_registration_is_monotonic() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("transaction")).unwrap();
    fs::create_dir(root.path().join("other")).unwrap();
    fs::write(root.path().join("source.txt"), b"original").unwrap();
    fs::write(root.path().join("transaction/replacement"), b"replacement").unwrap();
    let parent = directory(root.path());
    let tx = directory(&root.path().join("transaction"));
    let other = directory(&root.path().join("other"));
    let original = File::open(root.path().join("source.txt")).unwrap();
    let replacement = File::open(root.path().join("transaction/replacement")).unwrap();
    let mut planned = WindowsIdentity::from_handles(&parent, &original).unwrap();
    planned.register_handles(&parent, &tx, None).unwrap();
    planned
        .register_handles(&parent, &tx, Some(&replacement))
        .unwrap();
    let saved = planned.clone();
    planned
        .register_handles(&parent, &tx, Some(&replacement))
        .unwrap();
    assert_eq!(planned, saved);
    assert!(
        planned
            .register_handles(&parent, &other, Some(&replacement))
            .is_err()
    );
    assert!(planned.register_handles(&parent, &tx, None).is_err());
    assert!(
        planned
            .register_handles(&parent, &tx, Some(&original))
            .is_err()
    );
    assert!(
        planned
            .register_handles(&other, &tx, Some(&replacement))
            .is_err()
    );
    assert!(
        planned
            .register_handles(&parent, &tx, Some(&parent))
            .is_err()
    );
    assert_eq!(planned, saved);
    let mut wrong_volume = saved.clone();
    wrong_volume.volume_serial ^= 1;
    let before = wrong_volume.clone();
    assert!(
        wrong_volume
            .register_handles(&parent, &tx, Some(&replacement))
            .is_err()
    );
    assert_eq!(wrong_volume, before);
    fs::rename(
        root.path().join("source.txt"),
        root.path().join("transaction/original"),
    )
    .unwrap();
    assert_eq!(
        file_identity(&original, FileKind::File).unwrap().file_id,
        saved.original_file_id
    );
    let reopened = File::open(root.path().join("transaction/original")).unwrap();
    assert_eq!(
        file_identity(&reopened, FileKind::File).unwrap(),
        file_identity(&original, FileKind::File).unwrap()
    );
    let encoded = serde_json::to_vec(&planned).unwrap();
    assert_eq!(
        serde_json::from_slice::<WindowsIdentity>(&encoded).unwrap(),
        saved
    );
}

#[test]
fn wrong_object_types_and_hardlinks_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("source.txt"), b"original").unwrap();
    let parent = directory(root.path());
    let original = File::open(root.path().join("source.txt")).unwrap();
    assert!(file_identity(&parent, FileKind::File).is_err());
    assert!(file_identity(&original, FileKind::Directory).is_err());
    fs::hard_link(
        root.path().join("source.txt"),
        root.path().join("alias.txt"),
    )
    .unwrap();
    assert!(file_identity(&original, FileKind::File).is_err());
    assert!(WindowsIdentity::from_handles(&parent, &original).is_err());
    assert_eq!(
        fs::read(root.path().join("source.txt")).unwrap(),
        b"original"
    );
}

#[test]
fn stream_inventory_observes_ads_and_bounds_total_without_claiming_content_stability() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.txt");
    fs::write(&path, b"main").unwrap();
    fs::write(root.path().join("source.txt:Zone.Identifier"), b"abc").unwrap();
    let file = File::open(&path).unwrap();
    assert!(stream_inventory(&file, 6).is_err());
    let before = stream_inventory(&file, 7).unwrap();
    assert_eq!(before.logical_bytes, 7);
    assert_eq!(before.streams.len(), 2);
    assert!(
        before
            .streams
            .iter()
            .any(|stream| stream.name == ":Zone.Identifier:$DATA" && stream.bytes == 3)
    );
    // Equal names and sizes do not establish equal stream content.
    fs::write(root.path().join("source.txt:Zone.Identifier"), b"xyz").unwrap();
    assert_eq!(stream_inventory(&file, 7).unwrap(), before);
    for index in 0..31 {
        fs::write(root.path().join(format!("source.txt:extra-{index}")), b"").unwrap();
    }
    assert!(stream_inventory(&file, 100).is_err());
    assert_eq!(fs::read(path).unwrap(), b"main");
}

#[test]
fn private_storage_rejects_unexpected_ads_without_deleting_them() {
    let root = tempfile::tempdir().unwrap();
    let parent = directory(root.path());
    let vault = PrivateDirectory::open_or_create(&parent, "vault").unwrap();
    let lock = vault.lock().unwrap();
    lock.create_new("payload", b"main").unwrap();
    let stream = root.path().join("vault/payload:unexpected");
    fs::write(&stream, b"extra material").unwrap();
    assert!(lock.read("payload", 100).is_err());
    assert_eq!(fs::read(&stream).unwrap(), b"extra material");
    fs::remove_file(stream).unwrap();
    assert_eq!(lock.read("payload", 100).unwrap(), b"main");
    lock.create_new("index-pending-retained", b"retained")
        .unwrap();
    let stream = root.path().join("vault/index-pending-retained:unexpected");
    fs::write(&stream, b"unaccounted").unwrap();
    assert!(lock.pending_index_inventory().is_err());
    assert_eq!(
        lock.write_index(b"must not publish").unwrap_err().outcome,
        IndexWriteOutcome::NotPublished
    );
    assert!(!root.path().join("vault/index.json").exists());
    assert_eq!(fs::read(stream).unwrap(), b"unaccounted");
}

#[test]
fn private_directory_is_created_reopened_and_pinned_with_its_children() {
    let root = tempfile::tempdir().unwrap();
    let parent = directory(root.path());
    let vault = PrivateDirectory::open_or_create(&parent, "vault").unwrap();
    vault.validate().unwrap();
    let child = vault.open_child("子目录 1").unwrap();
    child.validate().unwrap();
    let id = file_identity(vault.handle(), FileKind::Directory).unwrap();
    assert_eq!(
        PrivateDirectory::create_new(&parent, "vault")
            .err()
            .unwrap()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    let fresh = PrivateDirectory::create_new(&parent, "new transaction").unwrap();
    fresh.validate().unwrap();
    let fresh_id = file_identity(fresh.handle(), FileKind::Directory).unwrap();
    assert!(PrivateDirectory::create_new(&parent, "new transaction").is_err());
    assert_eq!(
        file_identity(fresh.handle(), FileKind::Directory).unwrap(),
        fresh_id
    );
    assert!(fs::rename(root.path().join("vault"), root.path().join("moved")).is_err());
    assert!(
        fs::rename(
            root.path().join("vault/子目录 1"),
            root.path().join("moved-child")
        )
        .is_err()
    );
    let reopened = PrivateDirectory::open_or_create(&parent, "vault").unwrap();
    assert_eq!(
        file_identity(reopened.handle(), FileKind::Directory).unwrap(),
        id
    );
    for name in [
        "../escape",
        "nested/child",
        "name:stream",
        "CON",
        "NUL.txt",
        "trailing.",
        "",
    ] {
        assert!(vault.open_child(name).is_err());
    }
    assert!(security::validate_private(vault.handle(), "S-1-5-19").is_err());
    drop((child, reopened, vault));
    let reopened = PrivateDirectory::open_or_create(&parent, "vault").unwrap();
    reopened.validate().unwrap();
    assert_eq!(
        file_identity(reopened.handle(), FileKind::Directory).unwrap(),
        id
    );
}

#[test]
fn existing_nonprivate_directory_is_rejected_without_repairing_it() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("existing")).unwrap();
    fs::write(root.path().join("existing/sentinel"), b"unchanged").unwrap();
    let parent = directory(root.path());
    for _ in 0..2 {
        assert!(PrivateDirectory::open_or_create(&parent, "existing").is_err());
    }
    assert_eq!(
        fs::read(root.path().join("existing/sentinel")).unwrap(),
        b"unchanged"
    );
}

#[test]
fn changed_private_dacl_blocks_reopen_and_child_creation() {
    use ::windows::Win32::Security::{Authorization::*, *};
    let root = tempfile::tempdir().unwrap();
    let parent = directory(root.path());
    let vault = PrivateDirectory::open_or_create(&parent, "vault").unwrap();
    assert!(
        transaction_location(root.path(), &parent, "vault", Some(vault.handle()), None).is_ok()
    );
    let setter = OpenOptions::new()
        .access_mode((READ_CONTROL | WRITE_DAC).0)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
        .open(root.path().join("vault"))
        .unwrap();
    let user = security::current_user().unwrap();
    let descriptor = security::Descriptor::from_sddl(&format!("D:P(A;;FA;;;{user})")).unwrap();
    let mut present = ::windows::core::BOOL::default();
    let mut defaulted = ::windows::core::BOOL::default();
    let mut dacl = std::ptr::null_mut();
    unsafe {
        GetSecurityDescriptorDacl(descriptor.pointer, &mut present, &mut dacl, &mut defaulted)
            .unwrap();
        SetSecurityInfo(
            HANDLE(setter.as_raw_handle()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl),
            None,
        )
        .ok()
        .unwrap();
    }
    assert!(vault.validate().is_err());
    assert!(
        transaction_location(root.path(), &parent, "vault", Some(vault.handle()), None).is_err()
    );
    assert!(vault.open_child("must-not-exist").is_err());
    assert!(!root.path().join("vault/must-not-exist").exists());
    assert!(PrivateDirectory::open_or_create(&parent, "vault").is_err());
}

#[test]
fn private_lock_reports_busy_and_pins_directory_until_guard_drop() {
    let root = tempfile::tempdir().unwrap();
    let parent = directory(root.path());
    let vault = PrivateDirectory::open_or_create(&parent, "vault").unwrap();
    let first = vault.try_lock().unwrap().unwrap();
    assert!(vault.try_lock().unwrap().is_none());
    assert!(
        fs::rename(
            root.path().join("vault/lock"),
            root.path().join("other-lock")
        )
        .is_err()
    );
    drop(first);
    let second = vault.try_lock().unwrap().unwrap();
    drop(vault);
    assert!(fs::rename(root.path().join("vault"), root.path().join("moved")).is_err());
    drop(second);
    fs::rename(root.path().join("vault"), root.path().join("moved")).unwrap();
    let reopened = PrivateDirectory::open_or_create(&parent, "moved").unwrap();
    assert!(reopened.try_lock().unwrap().is_some());
}

#[test]
fn private_lock_rejects_nonprivate_existing_file() {
    let root = tempfile::tempdir().unwrap();
    let parent = directory(root.path());
    fs::write(root.path().join("untrusted"), b"unchanged").unwrap();
    let vault = PrivateDirectory::open_or_create(&parent, "vault").unwrap();
    fs::rename(
        root.path().join("untrusted"),
        root.path().join("vault/lock"),
    )
    .unwrap();
    assert!(vault.try_lock().is_err());
    assert_eq!(
        fs::read(root.path().join("vault/lock")).unwrap(),
        b"unchanged"
    );
}

#[test]
fn private_storage_creates_syncs_reopens_and_refuses_overwrite() {
    let root = tempfile::tempdir().unwrap();
    let parent = directory(root.path());
    let vault = PrivateDirectory::open_or_create(&parent, "vault").unwrap();
    let lock = vault.try_lock().unwrap().unwrap();
    lock.create_new("material.bin", b"original material")
        .unwrap();
    assert_eq!(
        lock.read("material.bin", 100).unwrap(),
        b"original material"
    );
    assert_eq!(
        lock.create_new("material.bin", b"replacement")
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(
        lock.read("material.bin", 100).unwrap(),
        b"original material"
    );
    assert!(lock.read("material.bin", 4).is_err());
    assert_eq!(
        lock.read("absent", 100).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    assert!(!root.path().join("vault/absent").exists());
    drop((lock, vault));
    let vault = PrivateDirectory::open_or_create(&parent, "vault").unwrap();
    let lock = vault.try_lock().unwrap().unwrap();
    assert_eq!(
        lock.read("material.bin", 100).unwrap(),
        b"original material"
    );
}

#[test]
fn private_storage_rejects_untrusted_material_and_dangerous_names() {
    let root = tempfile::tempdir().unwrap();
    let parent = directory(root.path());
    let vault = PrivateDirectory::open_or_create(&parent, "vault").unwrap();
    let lock = vault.try_lock().unwrap().unwrap();
    for name in ["../outside", "file:stream", "nested/file", "lock", "LOCK"] {
        assert!(lock.create_new(name, b"must not write").is_err());
        assert!(lock.read(name, 100).is_err());
    }
    fs::write(root.path().join("untrusted"), b"unchanged").unwrap();
    fs::rename(
        root.path().join("untrusted"),
        root.path().join("vault/untrusted"),
    )
    .unwrap();
    assert!(lock.read("untrusted", 100).is_err());
    assert_eq!(
        fs::read(root.path().join("vault/untrusted")).unwrap(),
        b"unchanged"
    );
}

#[test]
fn private_material_read_rejects_active_writer_and_added_hardlink() {
    let root = tempfile::tempdir().unwrap();
    let parent = directory(root.path());
    let vault = PrivateDirectory::open_or_create(&parent, "vault").unwrap();
    let lock = vault.try_lock().unwrap().unwrap();
    lock.create_new("material", b"unchanged").unwrap();
    let writer = OpenOptions::new()
        .write(true)
        .open(root.path().join("vault/material"))
        .unwrap();
    assert!(lock.read("material", 100).is_err());
    drop(writer);
    assert_eq!(lock.read("material", 100).unwrap(), b"unchanged");
    fs::hard_link(
        root.path().join("vault/material"),
        root.path().join("alias"),
    )
    .unwrap();
    assert!(lock.read("material", 100).is_err());
    assert_eq!(
        fs::read(root.path().join("vault/material")).unwrap(),
        b"unchanged"
    );
}

#[test]
fn private_index_replacement_reopens_as_complete_new_content() {
    let root = tempfile::tempdir().unwrap();
    let parent = directory(root.path());
    let vault = PrivateDirectory::open_or_create(&parent, "vault").unwrap();
    let lock = vault.try_lock().unwrap().unwrap();
    lock.write_index(br#"{"generation":1,"padding":"long initial value"}"#)
        .unwrap();
    let before = file_identity(
        &File::open(root.path().join("vault/index.json")).unwrap(),
        FileKind::File,
    )
    .unwrap();
    let replacement = br#"{"generation":2}"#;
    lock.write_index(replacement).unwrap();
    assert_eq!(lock.read("index.json", 100).unwrap(), replacement);
    let after = file_identity(
        &File::open(root.path().join("vault/index.json")).unwrap(),
        FileKind::File,
    )
    .unwrap();
    assert_ne!(before.file_id, after.file_id);
    assert_eq!(fs::read_dir(root.path().join("vault")).unwrap().count(), 2);
    drop((lock, vault));
    let vault = PrivateDirectory::open_or_create(&parent, "vault").unwrap();
    let lock = vault.try_lock().unwrap().unwrap();
    assert_eq!(lock.read("index.json", 100).unwrap(), replacement);
}

#[test]
fn private_index_failure_after_rename_attempt_blocks_writes_until_reopen() {
    let root = tempfile::tempdir().unwrap();
    let parent = directory(root.path());
    let vault = PrivateDirectory::open_or_create(&parent, "vault").unwrap();
    let lock = vault.try_lock().unwrap().unwrap();
    lock.write_index(b"old index").unwrap();
    let reader = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .open(root.path().join("vault/index.json"))
        .unwrap();
    let error = lock.write_index(b"new index").unwrap_err();
    assert_eq!(error.outcome, IndexWriteOutcome::OutcomeUnknown);
    assert_eq!(lock.read("index.json", 100).unwrap(), b"old index");
    drop(reader);
    let count = fs::read_dir(root.path().join("vault")).unwrap().count();
    assert_eq!(count, 3);
    let pending = lock.pending_index_inventory().unwrap();
    assert_eq!(pending.bytes, 9);
    assert_eq!(pending.files.len(), 1);
    assert_eq!(
        lock.read(&pending.files[0].name, 100).unwrap(),
        b"new index"
    );
    assert_eq!(
        lock.write_index(b"must not retry").unwrap_err().outcome,
        IndexWriteOutcome::OutcomeUnknown
    );
    assert!(lock.create_new("must-not-exist", b"x").is_err());
    assert_eq!(
        fs::read_dir(root.path().join("vault")).unwrap().count(),
        count
    );
    drop(lock);
    let lock = vault.try_lock().unwrap().unwrap();
    assert_eq!(lock.pending_index_inventory().unwrap(), pending);
    assert_eq!(lock.read("index.json", 100).unwrap(), b"old index");
    assert_eq!(
        lock.write_index_with_limit(b"reopened index", 31)
            .unwrap_err()
            .outcome,
        IndexWriteOutcome::NotPublished
    );
    assert_eq!(
        fs::read_dir(root.path().join("vault")).unwrap().count(),
        count
    );
    assert_eq!(lock.read("index.json", 100).unwrap(), b"old index");
    assert_eq!(lock.pending_index_inventory().unwrap(), pending);
    // Old (9), pending (9), and new (14) bytes all count at peak publication.
    lock.write_index_with_limit(b"reopened index", 32).unwrap();
    assert_eq!(lock.read("index.json", 100).unwrap(), b"reopened index");
}

#[test]
fn record_cleanup_removes_only_selected_material_and_can_resume() {
    let root = tempfile::tempdir().unwrap();
    let parent = directory(root.path());
    let vault = PrivateDirectory::open_or_create(&parent, "vault").unwrap();
    let lock = vault.try_lock().unwrap().unwrap();
    let id = "a".repeat(64);
    let other = format!("{}.body", "b".repeat(64));
    lock.write_index(b"purging record").unwrap();
    lock.create_new(&other, b"other record").unwrap();
    lock.create_new("index-pending-retained", b"unresolved index")
        .unwrap();
    for suffix in ["body", "metadata", "body.tmp", "metadata.tmp"] {
        lock.create_new(&format!("{id}.{suffix}"), b"backup")
            .unwrap();
    }
    let metadata = root.path().join(format!("vault/{id}.metadata"));
    let writer = OpenOptions::new().write(true).open(&metadata).unwrap();
    assert!(lock.remove_record_material(&id).is_err());
    // A failure after deleting the body must preserve the remaining materials.
    assert!(!root.path().join(format!("vault/{id}.body")).exists());
    assert_eq!(fs::read(&metadata).unwrap(), b"backup");
    drop(writer);
    let reader = OpenOptions::new()
        .read(true)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .open(&metadata)
        .unwrap();
    // Even a delete-sharing reader must not outlive a successful cleanup receipt.
    assert!(lock.remove_record_material(&id).is_err());
    assert_eq!(fs::read(&metadata).unwrap(), b"backup");
    drop(reader);
    lock.remove_record_material(&id).unwrap();
    lock.remove_record_material(&id).unwrap();
    for suffix in ["body", "metadata", "body.tmp", "metadata.tmp"] {
        assert!(!root.path().join(format!("vault/{id}.{suffix}")).exists());
    }
    assert_eq!(lock.read("index.json", 100).unwrap(), b"purging record");
    assert_eq!(lock.read(&other, 100).unwrap(), b"other record");
    assert_eq!(
        lock.read("index-pending-retained", 100).unwrap(),
        b"unresolved index"
    );
    for invalid in ["index", "../index.json", "", &"A".repeat(64)] {
        assert!(lock.remove_record_material(invalid).is_err());
    }
}

#[test]
fn record_cleanup_rejects_hardlinked_or_untrusted_material() {
    let root = tempfile::tempdir().unwrap();
    let parent = directory(root.path());
    let vault = PrivateDirectory::open_or_create(&parent, "vault").unwrap();
    let lock = vault.try_lock().unwrap().unwrap();
    let id = "a".repeat(64);
    let name = format!("{id}.body");
    lock.create_new(&name, b"backup").unwrap();
    let path = root.path().join("vault").join(&name);
    fs::hard_link(&path, root.path().join("alias")).unwrap();
    assert!(lock.remove_record_material(&id).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"backup");
    assert_eq!(fs::read(root.path().join("alias")).unwrap(), b"backup");
    fs::remove_file(root.path().join("alias")).unwrap();
    lock.remove_record_material(&id).unwrap();
    let outside = root.path().join("outside");
    fs::write(&outside, b"not a private backup").unwrap();
    fs::rename(&outside, &path).unwrap();
    assert!(lock.remove_record_material(&id).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"not a private backup");
}

#[test]
fn pending_inventory_limit_blocks_new_staging_without_discarding_material() {
    let root = tempfile::tempdir().unwrap();
    let parent = directory(root.path());
    let vault = PrivateDirectory::open_or_create(&parent, "vault").unwrap();
    let lock = vault.try_lock().unwrap().unwrap();
    lock.write_index(b"old").unwrap();
    // Long names force the native directory enumeration across buffer boundaries.
    for index in 0..256 {
        let name = format!("index-pending-{index:03}-{}", "x".repeat(170));
        lock.create_new(&name, b"x").unwrap();
    }
    let pending = lock.pending_index_inventory().unwrap();
    assert_eq!(pending.files.len(), 256);
    assert_eq!(pending.bytes, 256);
    assert_eq!(
        lock.write_index_with_limit(b"new", u64::MAX)
            .unwrap_err()
            .outcome,
        IndexWriteOutcome::NotPublished
    );
    assert_eq!(lock.read("index.json", 100).unwrap(), b"old");
    assert_eq!(lock.pending_index_inventory().unwrap(), pending);
    lock.create_new("index-pending-over-limit", b"x").unwrap();
    assert!(lock.pending_index_inventory().is_err());
    assert_eq!(
        lock.write_index(b"new").unwrap_err().outcome,
        IndexWriteOutcome::NotPublished
    );
    assert_eq!(
        fs::read_dir(root.path().join("vault")).unwrap().count(),
        259
    );
}

#[test]
fn pending_inventory_rejects_untrusted_or_busy_material_instead_of_undercounting() {
    let root = tempfile::tempdir().unwrap();
    let parent = directory(root.path());
    let vault = PrivateDirectory::open_or_create(&parent, "vault").unwrap();
    let lock = vault.try_lock().unwrap().unwrap();
    assert_eq!(lock.pending_index_inventory().unwrap().bytes, 0);
    lock.create_new("index-pending-fixture", b"retained")
        .unwrap();
    let writer = OpenOptions::new()
        .write(true)
        .open(root.path().join("vault/index-pending-fixture"))
        .unwrap();
    assert!(lock.pending_index_inventory().is_err());
    assert_eq!(
        lock.write_index(b"must not publish").unwrap_err().outcome,
        IndexWriteOutcome::NotPublished
    );
    assert!(!root.path().join("vault/index.json").exists());
    drop(writer);
    assert_eq!(lock.pending_index_inventory().unwrap().bytes, 8);
    fs::hard_link(
        root.path().join("vault/index-pending-fixture"),
        root.path().join("alias"),
    )
    .unwrap();
    assert!(lock.pending_index_inventory().is_err());
    assert_eq!(fs::read(root.path().join("alias")).unwrap(), b"retained");
}

#[test]
fn private_index_rejects_busy_or_untrusted_destination_before_publication() {
    let root = tempfile::tempdir().unwrap();
    let parent = directory(root.path());
    let vault = PrivateDirectory::open_or_create(&parent, "vault").unwrap();
    let lock = vault.try_lock().unwrap().unwrap();
    fs::write(root.path().join("untrusted"), b"unchanged").unwrap();
    fs::rename(
        root.path().join("untrusted"),
        root.path().join("vault/index.json"),
    )
    .unwrap();
    assert_eq!(
        lock.write_index(b"replacement").unwrap_err().outcome,
        IndexWriteOutcome::NotPublished
    );
    assert_eq!(
        fs::read(root.path().join("vault/index.json")).unwrap(),
        b"unchanged"
    );
    assert_eq!(fs::read_dir(root.path().join("vault")).unwrap().count(), 2);
    fs::remove_file(root.path().join("vault/index.json")).unwrap();
    lock.write_index(b"private index").unwrap();
    let writer = OpenOptions::new()
        .write(true)
        .open(root.path().join("vault/index.json"))
        .unwrap();
    assert_eq!(
        lock.write_index(b"replacement").unwrap_err().outcome,
        IndexWriteOutcome::NotPublished
    );
    drop(writer);
    assert_eq!(lock.read("index.json", 100).unwrap(), b"private index");
    lock.write_index(b"after writer closed").unwrap();
    assert_eq!(
        lock.read("index.json", 100).unwrap(),
        b"after writer closed"
    );
}
