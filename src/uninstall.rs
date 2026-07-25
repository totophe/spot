//! `spot --uninstall` — undo everything `install.sh` and `--init` did.
//!
//! The counterpart to `--init`. A tool that edits your shell rc on the way in
//! should be able to take itself back out; leaving people to find and delete a
//! snippet by hand is how dead blocks accumulate in dotfiles for years.
//!
//! Everything here is deliberately conservative: only the exact guarded block is
//! removed, every edited file is backed up first, and a symlink is only deleted
//! once it has been confirmed to point at us.

use std::fs;
use std::path::{Path, PathBuf};

pub const BEGIN: &str = "# >>> spot >>>";
pub const END: &str = "# <<< spot <<<";

/// Remove every `BEGIN..END` block from `content`.
///
/// Returns `None` when there was nothing to remove, so callers can leave
/// untouched files alone rather than rewriting them identically.
pub fn strip_block(content: &str) -> Option<String> {
    if !content.contains(BEGIN) {
        return None;
    }
    let mut out = String::with_capacity(content.len());
    let mut inside = false;
    let mut removed = false;
    for line in content.lines() {
        let t = line.trim();
        if t == BEGIN {
            inside = true;
            removed = true;
            continue;
        }
        if t == END {
            inside = false;
            continue;
        }
        if !inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !removed {
        return None;
    }
    // An unterminated block means a hand-edited rc; taking everything after the
    // marker would be destructive, so refuse rather than guess.
    if inside {
        return None;
    }
    Some(out)
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Shell rc files that `--init` output plausibly landed in.
fn rc_candidates() -> Vec<PathBuf> {
    let Some(h) = home() else { return Vec::new() };
    [
        ".bashrc",
        ".zshrc",
        ".profile",
        ".bash_profile",
        ".zprofile",
        ".zshenv",
    ]
    .iter()
    .map(|f| h.join(f))
    .filter(|p| p.exists())
    .collect()
}

pub fn run(dry_run: bool, force: bool, sessions: &[String]) -> i32 {
    let mut acted = false;
    let say = |what: &str| {
        if dry_run {
            println!("would {what}");
        } else {
            println!("{what}");
        }
    };

    if !sessions.is_empty() && !force {
        eprintln!("spot: {} session(s) still running:", sessions.len());
        for s in sessions {
            eprintln!("        {s}");
        }
        eprintln!(
            "\n      They would keep running with no tool left to reach them.\n\
             \x20     Drop them first, or pass --force to uninstall anyway."
        );
        return 1;
    }

    // 1. The shell rc snippet.
    for rc in rc_candidates() {
        let Ok(content) = fs::read_to_string(&rc) else {
            continue;
        };
        let Some(stripped) = strip_block(&content) else {
            if content.contains(BEGIN) {
                eprintln!(
                    "spot: {} has an unterminated spot block — leaving it alone, \
                     remove it by hand",
                    rc.display()
                );
            }
            continue;
        };
        acted = true;
        say(&format!("remove the spot block from {}", rc.display()));
        if dry_run {
            continue;
        }
        // Back up before touching anything the user hand-edits.
        let backup = rc.with_extension("spot-backup");
        if let Err(e) = fs::write(&backup, &content) {
            eprintln!("spot: could not back up {}: {e}", rc.display());
            continue;
        }
        if let Err(e) = fs::write(&rc, stripped) {
            eprintln!("spot: could not rewrite {}: {e}", rc.display());
        } else {
            println!("  (backup: {})", backup.display());
        }
    }

    // 2. The `stay` symlink, only if it really is ours.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let stay = dir.join("stay");
            if points_at(&stay, &exe) {
                acted = true;
                say(&format!("remove {}", stay.display()));
                if !dry_run {
                    if let Err(e) = fs::remove_file(&stay) {
                        eprintln!("spot: could not remove {}: {e}", stay.display());
                    }
                }
            }
        }
    }

    // 3. The update-check stamp.
    if let Some(cache) = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".cache")))
        .map(|c| c.join("spot"))
        .filter(|c| c.exists())
    {
        acted = true;
        say(&format!("remove {}", cache.display()));
        if !dry_run {
            let _ = fs::remove_dir_all(&cache);
        }
    }

    // 4. The binary itself, last. Unlinking a running executable is fine on
    //    unix — the inode outlives the name — so this is safe even though it is
    //    us. It will fail on a package-managed install, which is correct: the
    //    package manager owns that file.
    if let Ok(exe) = std::env::current_exe() {
        acted = true;
        say(&format!("remove {}", exe.display()));
        if !dry_run {
            if let Err(e) = fs::remove_file(&exe) {
                eprintln!(
                    "spot: could not remove {}: {e}\n\
                     \x20     If spot came from a package, uninstall it with your \
                     package manager.",
                    exe.display()
                );
            }
        }
    }

    if !acted {
        println!("🐾 Nothing to uninstall — no spot block, symlink or cache found.");
        return 0;
    }
    if dry_run {
        println!("\nDry run: nothing was changed. Re-run without --dry-run to apply.");
    } else {
        println!("\n🦴 Spot has gone home. Restart your shell to finish.");
    }
    0
}

fn points_at(link: &Path, exe: &Path) -> bool {
    match fs::symlink_metadata(link) {
        Ok(md) if md.file_type().is_symlink() => fs::canonicalize(link)
            .ok()
            .zip(fs::canonicalize(exe).ok())
            .map(|(a, b)| a == b)
            .unwrap_or(false),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SNIPPET: &str = "# >>> spot >>>\nif command -v spot; then spot; fi\n# <<< spot <<<";

    #[test]
    fn removes_the_block_and_keeps_everything_else() {
        let rc = format!("export PATH=/x\n{SNIPPET}\nalias ll='ls -l'\n");
        let out = strip_block(&rc).unwrap();
        assert!(!out.contains("spot"), "block should be gone: {out:?}");
        assert!(out.contains("export PATH=/x"));
        assert!(out.contains("alias ll='ls -l'"));
    }

    #[test]
    fn leaves_files_without_a_block_untouched() {
        assert!(strip_block("export PATH=/x\nalias ll='ls -l'\n").is_none());
    }

    #[test]
    fn removes_every_block_if_init_was_run_twice() {
        let rc = format!("a\n{SNIPPET}\nb\n{SNIPPET}\nc\n");
        let out = strip_block(&rc).unwrap();
        assert!(!out.contains("spot"));
        assert_eq!(out, "a\nb\nc\n");
    }

    #[test]
    fn refuses_an_unterminated_block_rather_than_truncating() {
        // A hand-mangled rc: taking everything after the marker would eat the
        // rest of the user's config.
        let rc = "a\n# >>> spot >>>\nspot\nalias precious='keep me'\n";
        assert!(strip_block(rc).is_none());
    }

    #[test]
    fn tolerates_indented_markers() {
        let rc = "a\n  # >>> spot >>>\n  spot\n  # <<< spot <<<\nb\n";
        assert_eq!(strip_block(rc).unwrap(), "a\nb\n");
    }
}
