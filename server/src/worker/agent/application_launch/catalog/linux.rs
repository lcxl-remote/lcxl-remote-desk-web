//! XDG desktop entries resolved against the selected user's trusted environment.
use super::{CatalogCollection, MAX_ENTRY_BYTES, collect_files};
use desk_agent_protocol::application_launch::{
    ApplicationCatalogEntry, ApplicationTarget, ApplicationTargetKind,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

/// Values come from the verified session environment, never the service's env.
pub(crate) struct DesktopEnvironment {
    pub(crate) home: PathBuf,
    pub(crate) data_home: Option<PathBuf>,
    pub(crate) data_dirs: Vec<PathBuf>,
    pub(crate) executable_dirs: Vec<PathBuf>,
    pub(crate) current_desktops: Vec<String>,
    pub(crate) locale: String,
}

pub(crate) fn enumerate(environment: &DesktopEnvironment) -> CatalogCollection {
    let mut collection = CatalogCollection::default();
    if !environment.home.is_absolute() {
        collection.warn("No verified user home directory");
        return collection;
    }
    let mut roots = vec![
        environment
            .data_home
            .clone()
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| environment.home.join(".local/share")),
    ];
    if environment.data_dirs.is_empty() {
        roots.extend([
            PathBuf::from("/usr/local/share"),
            PathBuf::from("/usr/share"),
        ]);
    } else {
        roots.extend(
            environment
                .data_dirs
                .iter()
                .filter(|path| path.is_absolute())
                .cloned(),
        );
    }
    let mut ids = BTreeSet::new();
    for root in roots {
        let root = root.join("applications");
        for path in collect_files(&root, "desktop", &mut collection) {
            let Ok(relative) = path.strip_prefix(&root) else {
                continue;
            };
            let id = relative.to_string_lossy().replace('/', "-");
            // User definitions, including Hidden=true tombstones, mask system entries.
            if !ids.insert(id.clone()) {
                continue;
            }
            let bytes = match std::fs::metadata(&path).and_then(|metadata| {
                if metadata.len() > MAX_ENTRY_BYTES {
                    return Err(std::io::Error::other("oversized desktop entry"));
                }
                std::fs::read_to_string(&path)
            }) {
                Ok(bytes) => bytes,
                Err(_) => {
                    collection.warn("An application desktop entry could not be parsed");
                    continue;
                }
            };
            match parse(&bytes, &id, environment) {
                Ok(Some(mut entry)) => {
                    entry.sources.push(path.to_string_lossy().into_owned());
                    collection.insert(entry);
                }
                Ok(None) => {}
                Err(_) => {
                    collection.warn("An application desktop entry is malformed or unsupported")
                }
            }
        }
    }
    collection
}

fn parse(
    text: &str,
    id: &str,
    environment: &DesktopEnvironment,
) -> Result<Option<ApplicationCatalogEntry>, &'static str> {
    let mut fields = BTreeMap::new();
    let mut in_entry = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or("invalid desktop entry field")?;
        if fields.insert(key.trim(), value).is_some() {
            return Err("duplicate desktop entry field");
        }
    }
    let field = |key: &str| fields.get(key).copied().unwrap_or("");
    if field("Type") != "Application" || field("Hidden") == "true" || field("NoDisplay") == "true" {
        return Ok(None);
    }
    let desktops = |value: &str| {
        value.split(';').any(|desktop| {
            environment
                .current_desktops
                .iter()
                .any(|current| current == desktop)
        })
    };
    if (!field("OnlyShowIn").is_empty() && !desktops(field("OnlyShowIn")))
        || (!field("NotShowIn").is_empty() && desktops(field("NotShowIn")))
    {
        return Ok(None);
    }
    if !field("TryExec").is_empty()
        && executable(&unescape(field("TryExec"))?, environment).is_none()
    {
        return Ok(None);
    }
    let base = unescape(field("Name"))?;
    if base.is_empty() {
        return Err("missing desktop entry name");
    }
    let localized = locale_keys(&environment.locale)
        .into_iter()
        .find_map(|locale| fields.get(format!("Name[{locale}]").as_str()).copied());
    let display_name = localized
        .map(unescape)
        .transpose()?
        .unwrap_or_else(|| base.clone());
    let mut aliases = vec![base, id.trim_end_matches(".desktop").into()];
    // Keep actual registered localized names; never generate translations.
    for (key, value) in &fields {
        if key.starts_with("Name[") {
            aliases.push(unescape(value)?);
        }
    }
    aliases.sort();
    aliases.dedup();
    aliases.retain(|value| value != &display_name);
    let template = field("Exec");
    let terminal = field("Terminal") == "true";
    let mut reason = None;
    let mut args = None;
    let mut target = None;
    if template.is_empty() {
        reason = Some(
            if field("DBusActivatable") == "true" {
                "D-Bus-only activation is unsupported"
            } else {
                "No executable is registered"
            }
            .into(),
        );
    } else {
        let words = exec_words(&unescape(template)?)?;
        if let Some(program) = words.first().filter(|program| !program.contains('%')) {
            target = executable(program, environment).map(|value| ApplicationTarget {
                kind: ApplicationTargetKind::Executable,
                value: value.to_string_lossy().into_owned(),
            });
            if target.is_none() {
                reason = Some("Registered executable is unavailable".into());
            }
        }
        if words.iter().any(|word| word.contains('%')) {
            reason.get_or_insert_with(|| {
                "Exec field codes require explicitly chosen arguments".into()
            });
        } else {
            args = Some(words.into_iter().skip(1).collect());
        }
        if terminal {
            reason = Some("Entry requires an explicitly selected terminal application".into());
        }
    }
    let cwd = if field("Path").is_empty() {
        None
    } else {
        Some(unescape(field("Path"))?)
    };
    Ok(Some(ApplicationCatalogEntry {
        display_name,
        aliases,
        target,
        suggested_args: args,
        argument_template: (!template.is_empty()).then(|| template.into()),
        suggested_cwd: cwd,
        sources: vec![],
        unsupported_reason: reason,
    }))
}

fn executable(value: &str, environment: &DesktopEnvironment) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = Path::new(value);
    let candidates = if path.is_absolute() {
        vec![path.to_path_buf()]
    } else if value.contains('/') {
        vec![]
    } else {
        environment
            .executable_dirs
            .iter()
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(value))
            .collect()
    };
    candidates.into_iter().find(|path| {
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

fn locale_keys(locale: &str) -> Vec<String> {
    let (locale, modifier) = locale
        .split_once('@')
        .map_or((locale, None), |(a, b)| (a, Some(b)));
    let locale = locale.split('.').next().unwrap_or(locale);
    let language = locale.split('_').next().unwrap_or(locale);
    let mut keys = Vec::new();
    if let Some(modifier) = modifier {
        keys.push(format!("{locale}@{modifier}"));
    }
    keys.push(locale.into());
    if let Some(modifier) = modifier {
        keys.push(format!("{language}@{modifier}"));
    }
    keys.push(language.into());
    keys.dedup();
    keys
}

fn unescape(value: &str) -> Result<String, &'static str> {
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        out.push(match chars.next() {
            Some('s') => ' ',
            Some('n') => '\n',
            Some('t') => '\t',
            Some('r') => '\r',
            Some('\\') => '\\',
            _ => return Err("invalid desktop escape"),
        });
    }
    if out.contains('\0') {
        return Err("NUL in desktop entry");
    }
    Ok(out)
}

/// Desktop Exec grammar is not shell syntax: no substitution or execution occurs.
fn exec_words(value: &str) -> Result<Vec<String>, &'static str> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut present = false;
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                quoted = !quoted;
                present = true;
            }
            '\\' => {
                word.push(chars.next().ok_or("trailing Exec escape")?);
                present = true;
            }
            ' ' | '\t' if !quoted => {
                if present {
                    words.push(std::mem::take(&mut word));
                    present = false;
                }
            }
            _ => {
                word.push(ch);
                present = true;
            }
        }
    }
    if quoted {
        return Err("unclosed Exec quote");
    }
    if present {
        words.push(word);
    }
    if words.is_empty() {
        return Err("empty Exec");
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn environment() -> DesktopEnvironment {
        DesktopEnvironment {
            home: "/home/user".into(),
            data_home: None,
            data_dirs: vec![],
            executable_dirs: vec![],
            current_desktops: vec!["GNOME".into()],
            locale: "zh_CN.UTF-8".into(),
        }
    }
    #[test]
    fn hidden_and_desktop_filters_are_applied_before_listing() {
        for extra in [
            "Hidden=true",
            "NoDisplay=true",
            "OnlyShowIn=KDE;",
            "NotShowIn=GNOME;",
        ] {
            assert!(
                parse(
                    &format!("[Desktop Entry]\nType=Application\nName=App\n{extra}"),
                    "app.desktop",
                    &environment()
                )
                .unwrap()
                .is_none()
            );
        }
    }
    #[test]
    fn localized_names_and_dbus_only_entries_are_preserved_without_fake_argv() {
        let entry = parse("[Desktop Entry]\nType=Application\nName=Calendar\nName[zh_CN]=日历\nDBusActivatable=true", "calendar.desktop", &environment()).unwrap().unwrap();
        assert_eq!(entry.display_name, "日历");
        assert!(entry.aliases.contains(&"Calendar".into()));
        assert!(entry.target.is_none());
        assert!(entry.suggested_args.is_none());
        assert!(entry.unsupported_reason.is_some());
    }
    #[test]
    fn exec_quotes_are_parsed_without_shell_expansion() {
        assert_eq!(
            exec_words("\"/opt/My App/app\" \"\" --name=hello $HOME %U").unwrap(),
            ["/opt/My App/app", "", "--name=hello", "$HOME", "%U"]
        );
        assert!(exec_words("\"unfinished").is_err());
        assert_eq!(
            locale_keys("sr_RS.UTF-8@latin"),
            ["sr_RS@latin", "sr_RS", "sr@latin", "sr"]
        );
    }
}
