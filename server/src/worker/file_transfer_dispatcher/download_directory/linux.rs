use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[cfg(target_os = "linux")]
pub(super) fn current() -> io::Result<PathBuf> {
    let home = crate::user_home::current().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "interactive user's home is unavailable",
        )
    })?;
    from_home(Path::new(&home))
}

fn from_home(home: &Path) -> io::Result<PathBuf> {
    let config = match fs::read_to_string(home.join(".config/user-dirs.dirs")) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(home.join("Downloads")),
        Err(error) => return Err(error),
    };
    parse(&config, home)
}

fn parse(config: &str, home: &Path) -> io::Result<PathBuf> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid XDG_DOWNLOAD_DIR");
    for line in config.lines().rev() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        if key.trim() != "XDG_DOWNLOAD_DIR" {
            continue;
        }
        let quoted = value
            .trim()
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .ok_or_else(invalid)?;
        // This is a data parser, not a shell. Only $HOME at the beginning is expanded.
        let expanded = if let Some(rest) = quoted.strip_prefix("$HOME") {
            if !rest.is_empty() && !rest.starts_with('/') {
                return Err(invalid());
            }
            format!("{}{rest}", home.display())
        } else {
            quoted.to_owned()
        };
        let mut decoded = String::new();
        let mut chars = expanded.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                decoded.push(chars.next().ok_or_else(invalid)?);
            } else {
                decoded.push(ch);
            }
        }
        let path = PathBuf::from(&decoded);
        if !path.is_absolute() || decoded.chars().any(char::is_control) {
            return Err(invalid());
        }
        return Ok(path);
    }
    Ok(home.join("Downloads"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uses_user_configuration_and_safe_home_expansion() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(from_home(dir.path()).unwrap(), dir.path().join("Downloads"));
        fs::create_dir(dir.path().join(".config")).unwrap();
        fs::write(
            dir.path().join(".config/user-dirs.dirs"),
            "XDG_DOWNLOAD_DIR=\"$HOME/My Downloads\"\n",
        )
        .unwrap();
        assert_eq!(
            from_home(dir.path()).unwrap(),
            dir.path().join("My Downloads")
        );
        assert_eq!(
            parse("XDG_DOWNLOAD_DIR=\"$HOME\"", dir.path()).unwrap(),
            dir.path()
        );
        assert_eq!(
            parse("XDG_DOWNLOAD_DIR=\"/mnt/shared\"", dir.path()).unwrap(),
            Path::new("/mnt/shared")
        );
        assert!(parse("XDG_DOWNLOAD_DIR=\"relative\"", dir.path()).is_err());
        assert!(parse("XDG_DOWNLOAD_DIR=$(touch /tmp/never)", dir.path()).is_err());
    }
}
