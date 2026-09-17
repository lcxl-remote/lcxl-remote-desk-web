//! Windows user-visible application registration and shortcut discovery.
use super::{CatalogCollection, MAX_CATALOG_FILES, collect_files};
use desk_agent_protocol::application_launch::{
    ApplicationCatalogEntry, ApplicationTarget, ApplicationTargetKind,
};
use std::path::Path;
use windows::{
    Win32::{
        Foundation::PROPERTYKEY,
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
                CoTaskMemFree, CoUninitialize, IPersistFile, STGM_READ,
                StructuredStorage::{PropVariantClear, PropVariantToStringAlloc},
            },
            Registry::{
                HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY,
                KEY_WOW64_64KEY, RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW,
                RegOpenKeyExW,
            },
        },
        UI::Shell::{
            BHID_EnumItems, FOLDERID_CommonStartMenu, FOLDERID_StartMenu, IEnumShellItems,
            IShellItem, IShellItem2, IShellLinkW, KNOWN_FOLDER_FLAG,
            PropertiesSystem::IPropertyStore, SHCreateItemFromParsingName, SHGetKnownFolderPath,
            SIGDN_NORMALDISPLAY, ShellLink,
        },
    },
    core::{GUID, Interface, PCWSTR, PWSTR},
};

const LINK_ARGUMENTS: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x436f2667_14e2_4feb_b30a_146c53b5b674),
    pid: 100,
};
const APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
    pid: 5,
};

struct ComApartment;
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}
struct RegistryKey(HKEY);
impl Drop for RegistryKey {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

pub(super) fn is_reparse_entry(entry: &std::fs::DirEntry) -> bool {
    use std::os::windows::fs::MetadataExt;
    entry
        .metadata()
        .map(|metadata| metadata.file_attributes() & 0x400 != 0)
        .unwrap_or(true)
}
fn owned_string(value: PWSTR) -> windows::core::Result<String> {
    let result = unsafe { value.to_string() };
    unsafe {
        CoTaskMemFree(Some(value.0.cast()));
    }
    Ok(result?)
}
fn buffer_string(value: &[u16]) -> windows::core::Result<String> {
    let end = value
        .iter()
        .position(|v| *v == 0)
        .ok_or_else(windows::core::Error::from_win32)?;
    String::from_utf16(&value[..end]).map_err(|_| windows::core::Error::from_win32())
}

/// Invoke on a dedicated thread in the already verified target user's context.
pub(crate) fn enumerate() -> CatalogCollection {
    let mut collection = CatalogCollection::default();
    if unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_err() {
        collection.warn("Application discovery COM initialization failed");
        return collection;
    }
    let _apartment = ComApartment;
    for folder in [FOLDERID_StartMenu, FOLDERID_CommonStartMenu] {
        let root = unsafe { SHGetKnownFolderPath(&folder, KNOWN_FOLDER_FLAG(0), None) }
            .and_then(owned_string);
        let root = match root {
            Ok(root) => root,
            Err(_) => {
                collection.warn("A Start Menu directory is unavailable");
                continue;
            }
        };
        for path in collect_files(Path::new(&root), "lnk", &mut collection) {
            match shortcut(&path) {
                Ok(entry) => collection.insert(entry),
                Err(_) => collection.warn("A Start Menu shortcut could not be resolved"),
            }
        }
    }
    app_paths(&mut collection);
    if packaged_apps(&mut collection).is_err() {
        collection.warn("The user's registered application folder is unavailable");
    }
    collection
}

fn shortcut(path: &Path) -> windows::core::Result<ApplicationCatalogEntry> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        let file: IPersistFile = link.cast()?;
        let name = wide(&path.to_string_lossy());
        file.Load(PCWSTR(name.as_ptr()), STGM_READ)?;
        // Resolve is deliberately not called: it can search the network or show UI.
        let mut target = vec![0u16; 32768];
        link.GetPath(&mut target, std::ptr::null_mut(), 0)?;
        let target = buffer_string(&target)?;
        let mut cwd = vec![0u16; 32768];
        let cwd = link
            .GetWorkingDirectory(&mut cwd)
            .and_then(|_| buffer_string(&cwd))
            .ok()
            .filter(|v| !v.is_empty());
        let properties: IPropertyStore = link.cast()?;
        let mut value = properties.GetValue(&LINK_ARGUMENTS)?;
        let arguments = PropVariantToStringAlloc(&value).and_then(owned_string);
        let _ = PropVariantClear(&mut value);
        let arguments = arguments.ok();
        let valid_target = !target.is_empty()
            && Path::new(&target).is_absolute()
            && Path::new(&target)
                .extension()
                .is_some_and(|v| v.eq_ignore_ascii_case("exe"));
        Ok(ApplicationCatalogEntry {
            display_name: path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            aliases: Path::new(&target)
                .file_stem()
                .map(|v| vec![v.to_string_lossy().into_owned()])
                .unwrap_or_default(),
            target: valid_target.then(|| ApplicationTarget {
                kind: ApplicationTargetKind::Executable,
                value: target,
            }),
            // Shortcut command-line strings have application-specific parsing.
            // Preserve the complete raw reference rather than inventing an argv split.
            suggested_args: arguments.as_ref().filter(|v| v.is_empty()).map(|_| vec![]),
            argument_template: arguments.filter(|v| !v.is_empty()),
            suggested_cwd: cwd,
            sources: vec![path.to_string_lossy().into_owned()],
            unsupported_reason: (!valid_target)
                .then(|| "Shortcut does not resolve to an explicit executable".into()),
        })
    }
}

fn app_paths(collection: &mut CatalogCollection) {
    let base = wide("Software\\Microsoft\\Windows\\CurrentVersion\\App Paths");
    for (hive, label) in [(HKEY_CURRENT_USER, "HKCU"), (HKEY_LOCAL_MACHINE, "HKLM")] {
        for view in [KEY_WOW64_64KEY, KEY_WOW64_32KEY] {
            let mut key = HKEY::default();
            let status = unsafe {
                RegOpenKeyExW(hive, PCWSTR(base.as_ptr()), None, KEY_READ | view, &mut key)
            };
            if status.0 == 2 {
                continue;
            }
            if status.is_err() {
                collection.warn("An App Paths registry view is unavailable");
                continue;
            }
            let key = RegistryKey(key);
            for index in 0..MAX_CATALOG_FILES as u32 {
                let mut name = [0u16; 256];
                let mut length = name.len() as u32;
                let status = unsafe {
                    RegEnumKeyExW(
                        key.0,
                        index,
                        Some(PWSTR(name.as_mut_ptr())),
                        &mut length,
                        None,
                        None,
                        None,
                        None,
                    )
                };
                if status.0 == 259 {
                    break;
                }
                if status.is_err() {
                    collection.warn("An App Paths entry could not be enumerated");
                    break;
                }
                if index + 1 == MAX_CATALOG_FILES as u32 {
                    collection.warn("App Paths enumeration limit reached");
                }
                let name = String::from_utf16_lossy(&name[..length as usize]);
                let subkey = wide(&name);
                let mut path = vec![0u16; 32768];
                let mut size = (path.len() * 2) as u32;
                let status = unsafe {
                    RegGetValueW(
                        key.0,
                        PCWSTR(subkey.as_ptr()),
                        PCWSTR::null(),
                        RRF_RT_REG_SZ,
                        None,
                        Some(path.as_mut_ptr().cast()),
                        Some(&mut size),
                    )
                };
                if status.is_err() {
                    collection.warn("An App Paths target could not be read");
                    continue;
                }
                let Ok(path) = buffer_string(&path) else {
                    collection.warn("An App Paths target is malformed");
                    continue;
                };
                let path = path.trim_matches('"');
                if !Path::new(path).is_absolute() {
                    collection.warn("An App Paths target is not absolute");
                    continue;
                }
                collection.insert(ApplicationCatalogEntry {
                    display_name: name.clone(),
                    aliases: vec![],
                    target: Some(ApplicationTarget {
                        kind: ApplicationTargetKind::Executable,
                        value: path.into(),
                    }),
                    suggested_args: Some(vec![]),
                    argument_template: None,
                    suggested_cwd: None,
                    sources: vec![format!("{label}\\App Paths\\{name}")],
                    unsupported_reason: None,
                });
            }
        }
    }
}

fn packaged_apps(collection: &mut CatalogCollection) -> windows::core::Result<()> {
    unsafe {
        let name = wide("shell:AppsFolder");
        let folder: IShellItem = SHCreateItemFromParsingName(PCWSTR(name.as_ptr()), None)?;
        let items: IEnumShellItems = folder.BindToHandler(None, &BHID_EnumItems)?;
        for index in 0..MAX_CATALOG_FILES {
            let mut batch = [None];
            let mut fetched = 0;
            items.Next(&mut batch, Some(&mut fetched))?;
            if fetched == 0 {
                break;
            }
            if index + 1 == MAX_CATALOG_FILES {
                collection.warn("Registered application enumeration limit reached");
            }
            let Some(item) = batch[0].take() else {
                continue;
            };
            let item2: IShellItem2 = item.cast()?;
            let id = match item2.GetString(&APP_USER_MODEL_ID).and_then(owned_string) {
                Ok(id) => id,
                Err(_) => continue,
            };
            // Unpackaged desktop entries are covered by Start Menu/App Paths.
            // Package AUMIDs contain the package family and application id.
            if !id.contains('!') {
                continue;
            }
            let display_name = item
                .GetDisplayName(SIGDN_NORMALDISPLAY)
                .and_then(owned_string)?;
            collection.insert(ApplicationCatalogEntry {
                display_name,
                aliases: vec![id.clone()],
                target: Some(ApplicationTarget {
                    kind: ApplicationTargetKind::WindowsAppId,
                    value: id,
                }),
                suggested_args: Some(vec![]),
                argument_template: None,
                suggested_cwd: None,
                sources: vec!["shell:AppsFolder".into()],
                unsupported_reason: None,
            });
        }
    }
    Ok(())
}
