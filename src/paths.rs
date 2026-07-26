//! Runtime directory, session names, socket discovery (RFC-0002 §2).
//!
//! There is no registry file: the sockets *are* the session list. When the last
//! session ends, `spot` leaves nothing behind but its own binary.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Where sockets live, in precedence order:
///   1. `$XDG_RUNTIME_DIR/spot/`
///   2. `$TMPDIR/spot-$UID/`
///   3. `/tmp/spot-$UID/`
///
/// Deliberately *not* `$XDG_CONFIG_HOME` (which RFC-0001 §1.2 specified): a
/// config directory survives reboot, so every crash and power cycle would leave
/// a stale socket behind forever. Runtime dirs are tmpfs and self-clean.
/// macOS has no `XDG_RUNTIME_DIR`, so option 2 carries it there.
pub fn runtime_dir() -> io::Result<PathBuf> {
    let uid = unsafe { libc::getuid() };
    let dir = if let Some(x) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        PathBuf::from(x).join("spot")
    } else if let Some(t) = std::env::var_os("TMPDIR").filter(|v| !v.is_empty()) {
        PathBuf::from(t).join(format!("spot-{uid}"))
    } else {
        PathBuf::from(format!("/tmp/spot-{uid}"))
    };

    match fs::symlink_metadata(&dir) {
        Ok(md) => {
            // Refuse to use a directory we do not own, or a symlink standing in
            // for one — both are how a shared /tmp gets you hijacked.
            if md.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{} is a symlink; refusing to use it", dir.display()),
                ));
            }
            if !md.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a directory", dir.display()),
                ));
            }
            use std::os::unix::fs::MetadataExt;
            if md.uid() != uid {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{} is owned by uid {}, not {uid}", dir.display(), md.uid()),
                ));
            }
            if md.permissions().mode() & 0o077 != 0 {
                fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(&dir)?;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        }
        Err(e) => return Err(e),
    }
    Ok(dir)
}

/// Session names are path components, so validation is what stops them being a
/// path traversal.
pub fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

pub fn socket_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.sock"))
}

pub fn lock_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.lock"))
}

pub fn err_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.err"))
}

/// Every session name with a socket present, sorted. Presence of the file says
/// nothing about liveness — see `crate::client::probe`.
pub fn list_names(dir: &Path) -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(stem) = name.strip_suffix(".sock") {
            if valid_name(stem) {
                out.push(stem.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Remove a session's leftovers.
///
/// Callers must hold the creation lock (see `reap_stale`). Unlinking a socket
/// because a *previous* connect failed is a time-of-check/time-of-use bug: the
/// socket may have been created in between, and removing a live one orphans a
/// running daemon.
///
/// **The lock file is deliberately never removed.** It is a mutex, not session
/// state, and mutual exclusion lives in its *inode*: unlinking it while another
/// client holds `flock` on it lets the next client create a fresh file, take an
/// uncontended lock on a different inode, and start a second daemon for the
/// same name. That is exactly what happened — a concurrent create produced
/// three daemons, two of them orphaned with unreachable children. A stray empty
/// lock file in a tmpfs runtime dir costs nothing by comparison.
pub fn cleanup(dir: &Path, name: &str) {
    let _ = fs::remove_file(socket_path(dir, name));
    let _ = fs::remove_file(err_path(dir, name));
}

/// Short, memorable default session names (adjective-noun), in the style of
/// `devcon`'s codename generator. Seeded from pid + time, which is plenty for
/// picking a label.
pub fn codename() -> String {
    const ADJ: &[&str] = &[
        "brave", "calm", "clever", "eager", "fuzzy", "gentle", "happy", "keen", "lucky", "merry",
        "nimble", "proud", "quiet", "swift", "tidy", "witty",
    ];
    const NOUN: &[&str] = &[
        "beagle", "collie", "corgi", "dingo", "husky", "kelpie", "lab", "mutt", "pointer", "pug",
        "setter", "shepherd", "spaniel", "terrier", "whippet", "wolf",
    ];
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seed = t ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!(
        "{}-{}",
        ADJ[(seed >> 8) as usize % ADJ.len()],
        NOUN[(seed >> 16) as usize % NOUN.len()]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_names() {
        for n in ["dev", "dev-box", "a", "build_2", "x.y", "A1"] {
            assert!(valid_name(n), "{n} should be valid");
        }
    }

    #[test]
    fn rejects_path_traversal_and_junk() {
        for n in [
            "",
            ".",
            "..",
            "../escape",
            "a/b",
            ".hidden",
            "-lead",
            "with space",
            "sock\0et",
        ] {
            assert!(!valid_name(n), "{n:?} should be rejected");
        }
    }

    #[test]
    fn rejects_overlong_names() {
        assert!(valid_name(&"a".repeat(64)));
        assert!(!valid_name(&"a".repeat(65)));
    }

    #[test]
    fn codename_is_a_valid_session_name() {
        assert!(valid_name(&codename()));
    }
}
