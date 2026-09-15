//! Lossless local-drive prefix conversion in device-resolved directory paths.
//! Central hosts use this on every OS; filesystem authority stays on the device.

pub(crate) fn differs_only_by_verbatim_prefix(requested: &str, canonical: &str) -> bool {
    let Some(plain) = canonical.strip_prefix(r"\\?\") else {
        return false;
    };
    if requested != plain {
        return false;
    }
    let bytes = plain.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || &bytes[1..3] != b":\\" {
        return false;
    }
    let tail = &plain[3..];
    tail.is_empty()
        || tail.split('\\').all(|part| {
            if part.is_empty()
                || matches!(part, "." | "..")
                || part.ends_with(['.', ' '])
                || part
                    .chars()
                    .any(|c| c.is_control() || "/:*?\"<>|".contains(c))
            {
                return false;
            }
            let lower = part.to_ascii_lowercase();
            let base = lower
                .split('.')
                .next()
                .unwrap_or_default()
                .trim_end_matches(' ');
            !matches!(base, "con" | "prn" | "aux" | "nul" | "conin$" | "conout$")
                && !["com", "lpt"].iter().any(|prefix| {
                    base.strip_prefix(prefix).is_some_and(|suffix| {
                        matches!(
                            suffix,
                            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                        )
                    })
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_lossless_local_drive_prefix_is_equivalent() {
        for path in [r"D:\", r"D:\测试 输入", r"C:\folder\file.with.dots"] {
            assert!(differs_only_by_verbatim_prefix(
                path,
                &format!(r"\\?\{path}")
            ));
        }
        for path in [
            r"D:relative",
            r"D:/folder",
            r"D:\a\..\b",
            r"D:\a\.",
            r"D:\a ",
            r"D:\a.",
            r"D:\a:stream",
            r"D:\\a",
            r"UNC\server\share",
            "/tmp/dir",
            r"D:\CON",
            r"D:\nul.txt",
            r"D:\LPT²",
            r"D:\com1 .txt",
        ] {
            assert!(
                !differs_only_by_verbatim_prefix(path, &format!(r"\\?\{path}")),
                "{path}"
            );
        }
        assert!(!differs_only_by_verbatim_prefix(
            r"D:\selected",
            r"\\?\D:\other"
        ));
        assert!(!differs_only_by_verbatim_prefix(
            r"D:\Selected",
            r"\\?\D:\selected"
        ));
        assert!(!differs_only_by_verbatim_prefix(
            r"D:\selected",
            r"\\?\E:\selected"
        ));
    }
}
