//! Archive path rules. The same rules apply on every platform, when an archive is
//! made and when it is extracted, so anything that was archived can be extracted
//! anywhere. They also close the usual holes: traversal, absolute paths, device
//! names, look-alike characters, case collisions.

use unicode_normalization::UnicodeNormalization;

use crate::error::{Error, Result};

pub const MAX_PATH_BYTES: usize = 65535;
pub const MAX_COMPONENT_BYTES: usize = 255;

fn unsafe_path<T>(msg: String) -> Result<T> {
    Err(Error::UnsafePath(msg))
}

fn is_windows_reserved(comp: &str) -> bool {
    let stem = comp.split('.').next().unwrap_or("");
    let up = stem.to_ascii_uppercase();
    match up.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" => true,
        _ => {
            if (up.starts_with("COM") || up.starts_with("LPT")) && up.len() == 4 {
                up.as_bytes()[3].is_ascii_digit()
                    || matches!(&up[3..], "\u{b9}" | "\u{b2}" | "\u{b3}")
            } else if (up.starts_with("COM") || up.starts_with("LPT")) && up.chars().count() == 4 {
                matches!(up.chars().nth(3), Some('\u{b9}' | '\u{b2}' | '\u{b3}'))
            } else {
                false
            }
        }
    }
}

/// Invisible or direction-changing characters that make one name look like another.
fn is_spoof_char(c: char) -> bool {
    matches!(c as u32, 0x200B..=0x200F | 0x202A..=0x202E | 0x2066..=0x2069 | 0xFEFF)
}

/// Validate an archive path and return its components.
pub fn check_portable_path(path: &str) -> Result<Vec<&str>> {
    if path.is_empty() {
        return unsafe_path("empty path".into());
    }
    if path.len() > MAX_PATH_BYTES {
        return unsafe_path(format!("path longer than {MAX_PATH_BYTES} bytes"));
    }
    if path.contains('\\') {
        return unsafe_path(format!("backslash in path (use '/'): {path:?}"));
    }
    if path.starts_with('/') {
        return unsafe_path(format!("absolute path: {path:?}"));
    }
    if path.nfc().collect::<String>() != path {
        return unsafe_path(format!("path is not NFC-normalised: {path:?}"));
    }
    let components: Vec<&str> = path.split('/').collect();
    for comp in &components {
        if comp.is_empty() || *comp == "." || *comp == ".." {
            return unsafe_path(format!("empty, '.' or '..' component in {path:?}"));
        }
        if comp.len() > MAX_COMPONENT_BYTES {
            return unsafe_path(format!(
                "name longer than {MAX_COMPONENT_BYTES} bytes in {path:?}"
            ));
        }
        for c in comp.chars() {
            let o = c as u32;
            if o < 0x20 || o == 0x7F {
                return unsafe_path(format!("control character in {path:?}"));
            }
            if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                return unsafe_path(format!(
                    "character {c:?} is not allowed in file names on Windows: {path:?}"
                ));
            }
            if is_spoof_char(c) {
                return unsafe_path(format!("invisible/bidi character U+{o:04X} in {path:?}"));
            }
            if (0xE000..=0xF8FF).contains(&o) || (0xF0000..=0x10FFFF).contains(&o) {
                return unsafe_path(format!("private-use character in {path:?}"));
            }
        }
        if comp.ends_with('.') || comp.ends_with(' ') {
            return unsafe_path(format!(
                "name ends with a dot or space (silently stripped on Windows): {path:?}"
            ));
        }
        if is_windows_reserved(comp) {
            return unsafe_path(format!(
                "reserved device name on Windows: {comp:?} in {path:?}"
            ));
        }
    }
    Ok(components)
}

/// Two paths with the same key would be the same file on a case-insensitive or
/// normalising filesystem (Windows, macOS).
pub fn collision_key(path: &str) -> String {
    path.nfkc().collect::<String>().to_lowercase()
}

/// Relative OS path -> archive path: '/'-separated, NFC.
pub fn archive_path_from_os(rel: &std::path::Path) -> String {
    let parts: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().nfc().collect::<String>()),
            _ => None,
        })
        .collect();
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_the_classics() {
        for bad in [
            "../x",
            "a/../b",
            "/abs",
            "C:/x",
            "a\\b",
            "a/./b",
            "",
            "a//b",
            "CON",
            "d/NUL.txt",
            "LPT1.x",
            "name.",
            "name ",
            "a:b",
            "a?b",
            "x|y",
            "tab\there",
            "a\u{202e}b",
            "z\u{200b}w",
        ] {
            assert!(
                check_portable_path(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
        let long = "a/".repeat(32768);
        assert!(check_portable_path(&long).is_err());
    }

    #[test]
    fn accepts_normal_and_long() {
        assert!(check_portable_path("docs/readme.txt").is_ok());
        assert!(check_portable_path("caf\u{e9}/r\u{e9}sum\u{e9}.pdf").is_ok());
        let long = format!("{}{}", "a".repeat(255).to_string() + "/", "b".repeat(255)).repeat(1);
        assert!(check_portable_path(&long).is_ok());
        let max = (0..255)
            .map(|_| "a".repeat(255))
            .collect::<Vec<_>>()
            .join("/")
            + "/"
            + &"b".repeat(255);
        assert!(max.len() <= MAX_PATH_BYTES);
        assert!(check_portable_path(&max).is_ok());
    }

    #[test]
    fn collisions() {
        assert_eq!(collision_key("Readme"), collision_key("readme"));
        assert_eq!(collision_key("fi"), collision_key("\u{fb01}"));
    }
}
