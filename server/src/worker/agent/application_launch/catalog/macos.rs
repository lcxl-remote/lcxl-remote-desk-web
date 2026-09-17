//! Installed app bundles and their actual localized metadata.
use super::{CatalogCollection, MAX_CATALOG_FILES};
use desk_agent_protocol::application_launch::{
    ApplicationCatalogEntry, ApplicationTarget, ApplicationTargetKind,
};
use objc2::{class, msg_send, rc::autoreleasepool, runtime::AnyObject};
use objc2_foundation::NSString;
use std::{
    ffi::CStr,
    path::{Path, PathBuf},
};

/// The user home is resolved by the authenticated native session host.
pub(crate) fn enumerate(user_home: &Path) -> CatalogCollection {
    autoreleasepool(|_| {
        let mut collection = CatalogCollection::default();
        if !user_home.is_absolute() {
            collection.warn("No verified user home directory");
            return collection;
        }
        let mut pending = vec![
            (PathBuf::from("/Applications"), 0),
            (user_home.join("Applications"), 0),
            (PathBuf::from("/System/Applications"), 0),
        ];
        let mut visited = 0;
        while let Some((directory, depth)) = pending.pop() {
            let entries = match std::fs::read_dir(directory) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => {
                    collection.warn("An Applications directory could not be read");
                    continue;
                }
            };
            for entry in entries {
                visited += 1;
                if visited > MAX_CATALOG_FILES {
                    collection.warn("Application directory traversal limit reached");
                    return collection;
                }
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(_) => {
                        collection.warn("An application entry could not be read");
                        continue;
                    }
                };
                let kind = match entry.file_type() {
                    Ok(kind) => kind,
                    Err(_) => {
                        collection.warn("Application entry metadata is unavailable");
                        continue;
                    }
                };
                if kind.is_symlink() {
                    collection.warn("Linked application entries were not traversed");
                    continue;
                }
                if !kind.is_dir() {
                    continue;
                }
                let path = entry.path();
                if path
                    .extension()
                    .is_some_and(|value| value.eq_ignore_ascii_case("app"))
                {
                    match bundle_entry(&path) {
                        Some(entry) => collection.insert(entry),
                        None => collection.warn("An application bundle could not be resolved"),
                    }
                    // Nested helper apps inside a bundle are not user-visible catalog entries.
                } else if depth < 8 {
                    pending.push((path, depth + 1));
                } else {
                    collection.warn("Application directory depth limit reached");
                }
            }
        }
        collection
    })
}

fn text(object: *mut AnyObject) -> Option<String> {
    if object.is_null() {
        return None;
    }
    unsafe {
        let is_string: bool = msg_send![object, isKindOfClass: class!(NSString)];
        if !is_string {
            return None;
        }
        let bytes: *const std::ffi::c_char = msg_send![object, UTF8String];
        if bytes.is_null() {
            return None;
        }
        CStr::from_ptr(bytes).to_str().ok().map(str::to_owned)
    }
}

fn bundle_entry(path: &Path) -> Option<ApplicationCatalogEntry> {
    let path_text = path.to_str()?;
    let native_path = NSString::from_str(path_text);
    unsafe {
        let bundle: *mut AnyObject = msg_send![class!(NSBundle), bundleWithPath: &*native_path];
        if bundle.is_null() {
            return None;
        }
        let info: *mut AnyObject = msg_send![bundle, infoDictionary];
        if info.is_null() {
            return None;
        }
        let package_key = NSString::from_str("CFBundlePackageType");
        let package_type: *mut AnyObject = msg_send![info, objectForKey: &*package_key];
        if text(package_type).as_deref() != Some("APPL") {
            return None;
        }
        let identifier: *mut AnyObject = msg_send![bundle, bundleIdentifier];
        let executable: *mut AnyObject = msg_send![bundle, executablePath];
        let name_key = NSString::from_str("CFBundleName");
        let name: *mut AnyObject = msg_send![info, objectForKey: &*name_key];
        let display_key = NSString::from_str("CFBundleDisplayName");
        let display: *mut AnyObject = msg_send![bundle, objectForInfoDictionaryKey: &*display_key];
        let localized_name: *mut AnyObject =
            msg_send![bundle, objectForInfoDictionaryKey: &*name_key];
        let display_name = text(display).or_else(|| text(localized_name)).or_else(|| {
            path.file_stem()
                .and_then(|value| value.to_str())
                .map(str::to_owned)
        })?;
        let mut aliases: Vec<_> = [text(identifier), text(name), text(executable)]
            .into_iter()
            .flatten()
            .collect();
        aliases.sort();
        aliases.dedup();
        aliases.retain(|value| value != &display_name);
        Some(ApplicationCatalogEntry {
            display_name,
            aliases,
            target: Some(ApplicationTarget {
                kind: ApplicationTargetKind::MacosBundle,
                value: path_text.into(),
            }),
            suggested_args: Some(vec![]),
            argument_template: None,
            suggested_cwd: None,
            sources: vec![path_text.into()],
            unsupported_reason: None,
        })
    }
}
