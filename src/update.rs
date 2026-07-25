//! omz-style self-update against GitHub Releases. Ported from `tmosh`.
//!
//! A throttled background check (at most once per 24h) writes a stamp file and
//! never blocks login. `--update` forces an immediate check + install.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const REPO: &str = "totophe/spot";
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

fn stamp_path() -> Option<PathBuf> {
    let base = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("spot").join("last-update-check"))
}

fn due_for_check() -> bool {
    let Some(path) = stamp_path() else {
        return false;
    };
    let Ok(meta) = fs::metadata(&path) else {
        return true;
    };
    match meta.modified().ok().and_then(|m| m.elapsed().ok()) {
        Some(elapsed) => elapsed >= CHECK_INTERVAL,
        None => true,
    }
}

fn touch_stamp() {
    if let Some(path) = stamp_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&path, b"");
    }
}

/// Spawn a detached `spot --self-update-bg`; returns immediately.
pub fn maybe_check_in_background() {
    if !due_for_check() {
        return;
    }
    if let Ok(exe) = env::current_exe() {
        let _ = Command::new(exe)
            .arg("--self-update-bg")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

pub fn run_background(current: &str) {
    touch_stamp();
    if let Ok(Some(latest)) = fetch_latest_tag() {
        if is_newer(&latest, current) {
            let _ = install(&latest);
        }
    }
}

pub fn run_foreground(current: &str) -> i32 {
    touch_stamp();
    eprintln!("spot {current}: checking for updates…");
    match fetch_latest_tag() {
        Ok(Some(latest)) if is_newer(&latest, current) => {
            eprintln!("Updating to {latest}…");
            match install(&latest) {
                Ok(()) => {
                    eprintln!("Updated to {latest}.");
                    0
                }
                Err(e) => {
                    eprintln!("Update failed: {e}");
                    1
                }
            }
        }
        Ok(_) => {
            eprintln!("Already up to date ({current}).");
            0
        }
        Err(e) => {
            eprintln!("Update check failed: {e}");
            1
        }
    }
}

fn fetch_latest_tag() -> io::Result<Option<String>> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let out = Command::new("curl")
        .args([
            "-fsSL",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: spot-updater",
            &url,
        ])
        .output()?;
    if !out.status.success() {
        return Ok(None);
    }
    let body = String::from_utf8_lossy(&out.stdout);
    Ok(extract_json_string(&body, "tag_name"))
}

/// Minimal extractor for a top-level `"key": "value"`, avoiding a serde
/// dependency for one lookup.
fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = json.find(&needle)? + needle.len();
    let rest = &json[start..];
    let colon = rest.find(':')?;
    let after = &rest[colon + 1..];
    let q1 = after.find('"')?;
    let after_q = &after[q1 + 1..];
    let q2 = after_q.find('"')?;
    Some(after_q[..q2].to_string())
}

fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.trim_start_matches('v')
            .split('.')
            .map(|p| {
                p.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
            })
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let (a, b) = (parse(latest), parse(current));
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

fn asset_name() -> &'static str {
    if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        "spot-x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
        "spot-aarch64-unknown-linux-gnu"
    } else if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        "spot-aarch64-apple-darwin"
    } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
        "spot-x86_64-apple-darwin"
    } else {
        "spot-unknown"
    }
}

fn install(tag: &str) -> io::Result<()> {
    let asset = asset_name();
    let url = format!("https://github.com/{REPO}/releases/download/{tag}/{asset}");

    let exe = env::current_exe()?;
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".spot-update-{tag}"));

    let out = Command::new("curl")
        .args(["-fsSL", "-o"])
        .arg(&tmp)
        .arg(&url)
        .output()?;
    if !out.status.success() {
        let _ = fs::remove_file(&tmp);
        return Err(io::Error::other(format!(
            "download failed for {asset}@{tag}"
        )));
    }
    if fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0) == 0 {
        let _ = fs::remove_file(&tmp);
        return Err(io::Error::other("downloaded asset was empty"));
    }
    set_executable(&tmp)?;
    // Atomic on the same filesystem; replaces a running binary safely on unix.
    fs::rename(&tmp, &exe).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    Ok(())
}

fn set_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)
}

pub fn flush() {
    let _ = io::stderr().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_compares_semver() {
        assert!(is_newer("v0.2.0", "v0.1.0"));
        assert!(is_newer("v1.0.0", "v0.9.9"));
        assert!(is_newer("0.1.1", "v0.1.0"));
        assert!(!is_newer("v0.1.0", "v0.1.0"));
        assert!(!is_newer("v0.1.0", "v0.2.0"));
        assert!(!is_newer("v0.1.0", "v0.1"));
    }

    #[test]
    fn extracts_tag_name_from_json() {
        let json = r#"{"url":"x","tag_name":"v1.2.3","name":"rel"}"#;
        assert_eq!(
            extract_json_string(json, "tag_name").as_deref(),
            Some("v1.2.3")
        );
        assert_eq!(extract_json_string(json, "missing"), None);
    }

    #[test]
    fn asset_name_is_prefixed() {
        assert!(asset_name().starts_with("spot-"));
    }
}
