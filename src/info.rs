//! Telegram-independent helpers backing the `info` command caption.

use std::io::Read;

use crate::auth::SelfIdentity;

pub const INFO_MEDIA_URL: &str =
    "https://tangled.org/zumuvik.tngl.sh/lavis/raw/main/assets/lavis-info.png";

/// Picks the most readable owner label in order: username, display name, then
/// the numeric account id.
pub fn owner_label(identity: &SelfIdentity) -> String {
    if let Some(username) = identity
        .username
        .as_deref()
        .filter(|username| !username.is_empty())
    {
        return format!("@{username}");
    }
    if let Some(display_name) = identity
        .display_name
        .as_deref()
        .filter(|display_name| !display_name.is_empty())
    {
        return display_name.to_owned();
    }
    identity.id.to_string()
}

/// Maps the `LAVIS_HOST` marker set by each packaging entry point to a
/// human-readable deployment label. The label reflects how the process was
/// started, not where it is physically deployed.
pub fn deployment_label(host_marker: Option<&str>) -> &'static str {
    match host_marker {
        Some("nixos-module") => "NixOS module",
        Some("nix-run") => "nix run",
        Some("nix-package") => "Nix package",
        _ => "standalone",
    }
}

const MAX_OS_RELEASE_READ_BYTES: usize = 16 * 1024;
const MAX_PRETTY_NAME_CHARS: usize = 128;

/// Parses `PRETTY_NAME` from the contents of an os-release file, handling
/// double-quoted, single-quoted, and unquoted values per os-release(5).
/// Returns `None` for a missing `PRETTY_NAME`, an empty value, or non-UTF-8
/// contents.
pub fn parse_pretty_name(contents: &[u8]) -> Option<String> {
    if contents.len() > MAX_OS_RELEASE_READ_BYTES {
        return None;
    }
    let text = std::str::from_utf8(contents).ok()?;
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("PRETTY_NAME=") else {
            continue;
        };
        let mut value = rest.trim();
        if value.len() >= 2 {
            let first = value.as_bytes()[0];
            let last = value.as_bytes()[value.len() - 1];
            if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
                value = &value[1..value.len() - 1];
            }
        }
        if value.is_empty() {
            return None;
        }
        return Some(value.chars().take(MAX_PRETTY_NAME_CHARS).collect());
    }
    None
}

/// Reads and parses the OS pretty name from `/etc/os-release` with a bounded
/// read. Returns `None` when the file is missing or unparseable.
pub fn read_os_release_pretty_name() -> Option<String> {
    let file = std::fs::File::open("/etc/os-release").ok()?;
    let mut contents = Vec::new();
    file.take(MAX_OS_RELEASE_READ_BYTES as u64)
        .read_to_end(&mut contents)
        .ok()?;
    parse_pretty_name(&contents)
}

/// Truncates a revision to the conventional short form used by git UIs.
pub fn short_commit(rev: &str) -> &str {
    rev.get(..7).unwrap_or(rev)
}

/// Compile-time build revision, injected by the Nix build as `LAVIS_GIT_REV`.
/// Development builds without the variable report `unknown`.
pub fn build_rev() -> &'static str {
    option_env!("LAVIS_GIT_REV").unwrap_or("unknown")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_labels_map_every_marker() {
        assert_eq!(deployment_label(Some("nixos-module")), "NixOS module");
        assert_eq!(deployment_label(Some("nix-run")), "nix run");
        assert_eq!(deployment_label(Some("nix-package")), "Nix package");
        assert_eq!(deployment_label(None), "standalone");
        assert_eq!(deployment_label(Some("other")), "standalone");
    }

    #[test]
    fn parses_double_quoted_pretty_name() {
        let contents = b"NAME=\"NixOS\"\nPRETTY_NAME=\"NixOS 25.05 (Warbler)\"\n";
        assert_eq!(
            parse_pretty_name(contents).as_deref(),
            Some("NixOS 25.05 (Warbler)")
        );
    }

    #[test]
    fn parses_single_quoted_pretty_name() {
        assert_eq!(
            parse_pretty_name(b"PRETTY_NAME='Pop!_OS 22.04'\n").as_deref(),
            Some("Pop!_OS 22.04")
        );
    }

    #[test]
    fn parses_unquoted_pretty_name() {
        assert_eq!(
            parse_pretty_name(b"PRETTY_NAME=Arch Linux\n").as_deref(),
            Some("Arch Linux")
        );
    }

    #[test]
    fn missing_or_empty_pretty_name_is_none() {
        assert_eq!(parse_pretty_name(b"NAME=\"NixOS\"\n"), None);
        assert_eq!(parse_pretty_name(b"PRETTY_NAME=\"\"\n"), None);
        assert_eq!(parse_pretty_name(b""), None);
        assert_eq!(parse_pretty_name(b"\xff\xfe invalid utf8"), None);
    }

    #[test]
    fn oversized_pretty_name_is_truncated() {
        let long = format!("PRETTY_NAME=\"{}\"", "x".repeat(MAX_PRETTY_NAME_CHARS * 2));
        let parsed = parse_pretty_name(long.as_bytes()).unwrap();
        assert_eq!(parsed.chars().count(), MAX_PRETTY_NAME_CHARS);
    }

    #[test]
    fn short_commit_formats_revisions() {
        assert_eq!(
            short_commit("b1d18f8ef407d043506c983b0d68e96c282eb1c9"),
            "b1d18f8"
        );
        assert_eq!(short_commit("unknown"), "unknown");
        assert_eq!(short_commit("abc"), "abc");
    }

    #[test]
    fn build_rev_is_never_empty_or_oversized() {
        assert!(!build_rev().is_empty());
        assert!(build_rev().len() <= 40);
    }
}
