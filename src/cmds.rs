//! Command surface: create/attach, stay, ls, drop (RFC-0001 §3).

use crate::client::{self, Outcome};
use crate::paths;
use crate::proto::*;
use crate::term;

use std::io::{ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct Options {
    pub keep_on_exit: bool,
    pub ring_bytes: usize,
    pub command: Vec<String>,
    pub steal: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            keep_on_exit: false,
            ring_bytes: 64 * 1024,
            command: Vec::new(),
            steal: false,
        }
    }
}

/// Parsed `STATUS_RESP` line.
pub struct Status {
    pub name: String,
    pub child_pid: i32,
    pub command: String,
    pub created: u64,
    pub attached: bool,
    pub state: String,
}

pub fn parse_status(line: &str) -> Option<Status> {
    let f: Vec<&str> = line.split('\t').collect();
    if f.len() < 10 {
        return None;
    }
    Some(Status {
        name: f[0].to_string(),
        child_pid: f[2].parse().unwrap_or(0),
        command: f[3].to_string(),
        created: f[4].parse().unwrap_or(0),
        attached: f[6] == "1",
        state: f[9].to_string(),
    })
}

/// Every live session, stale sockets cleaned up along the way.
pub fn sessions(dir: &Path) -> Vec<Status> {
    let mut out = Vec::new();
    for name in paths::list_names(dir).unwrap_or_default() {
        if let Some(line) = client::probe(dir, &name) {
            if let Some(s) = parse_status(&line) {
                out.push(s);
            }
        }
    }
    out
}

/// Attach to `name`, creating the session if it does not exist.
pub fn attach_or_create(dir: &Path, name: &str, opts: &Options) -> i32 {
    if let Some(code) = try_attach(dir, name, opts.steal) {
        return code;
    }
    match create(dir, name, opts) {
        Ok(()) => match try_attach(dir, name, opts.steal) {
            Some(code) => code,
            None => {
                eprintln!("spot: '{name}' vanished immediately after starting");
                1
            }
        },
        Err(e) => {
            eprintln!("spot: cannot start '{name}': {e}");
            1
        }
    }
}

/// Attach only — never creates. `spot fetch`.
pub fn fetch(dir: &Path, name: &str, opts: &Options) -> i32 {
    match try_attach(dir, name, opts.steal) {
        Some(code) => code,
        None => {
            eprintln!("spot: no session named '{name}'. `spot ls` shows what is running.");
            1
        }
    }
}

/// `None` means no daemon was listening.
fn try_attach(dir: &Path, name: &str, steal: bool) -> Option<i32> {
    let flags = if steal { FLAG_STEAL } else { 0 };
    let sock = match client::connect(dir, name, ROLE_ATTACH, flags) {
        Ok(Some(s)) => s,
        Ok(None) => return None,
        Err(e) => {
            eprintln!("spot: {e}");
            return Some(1);
        }
    };
    Some(report(client::attach(sock), name))
}

fn report(outcome: Outcome, name: &str) -> i32 {
    match outcome {
        Outcome::ChildExited(code) => code,
        Outcome::Detached => {
            eprintln!("🦮 Detached from '{name}'. Spot will stay!");
            0
        }
        Outcome::Stolen => {
            eprintln!("🐾 '{name}' was taken over by another client.");
            0
        }
        Outcome::Error(e) => {
            eprintln!("spot: {e}");
            1
        }
    }
}

/// Start a daemon for `name` and wait for its socket to appear.
///
/// The lock serialises concurrent creators; the double-check inside it means the
/// loser attaches to the winner's session instead of racing to bind.
fn create(dir: &Path, name: &str, opts: &Options) -> std::io::Result<()> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(paths::lock_path(dir, name))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Someone may have created it while we waited for the lock.
    if client::probe(dir, name).is_some() {
        return Ok(());
    }

    let exe = std::env::current_exe()?;
    let (cols, rows, _, _) = term::window_size();
    let mut cmd = Command::new(exe);
    cmd.arg("--daemon")
        .arg(name)
        .arg("--ring")
        .arg(opts.ring_bytes.to_string())
        .arg("--size")
        .arg(format!("{cols}x{rows}"));
    if opts.keep_on_exit {
        cmd.arg("--keep-on-exit");
    }
    if !opts.command.is_empty() {
        cmd.arg("--");
        cmd.args(&opts.command);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn()?;

    // Poll for the socket. The daemon's stdio is /dev/null, so a startup failure
    // arrives as a file rather than a message.
    let deadline = Instant::now() + Duration::from_secs(5);
    let errfile = paths::err_path(dir, name);
    while Instant::now() < deadline {
        if client::probe(dir, name).is_some() {
            return Ok(());
        }
        if let Ok(msg) = std::fs::read_to_string(&errfile) {
            if !msg.trim().is_empty() {
                let _ = std::fs::remove_file(&errfile);
                return Err(std::io::Error::other(msg.trim().to_string()));
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(std::io::Error::new(
        ErrorKind::TimedOut,
        "daemon did not come up within 5s",
    ))
}

/// `stay` — detach the current session, or a named one.
///
/// Resolution is deliberately strict: `$SPOT_SOCKET` or an explicit name, never
/// "the only session that happens to exist". A command that detaches something
/// different depending on how many sessions are running is not one you can trust.
pub fn stay(dir: &Path, name: Option<&str>) -> i32 {
    let sock: PathBuf = match name {
        Some(n) => paths::socket_path(dir, n),
        None => match std::env::var_os("SPOT_SOCKET") {
            Some(s) => PathBuf::from(s),
            None => {
                eprintln!("spot: not inside a spot session ($SPOT_SOCKET is unset).");
                let live = sessions(dir);
                if live.is_empty() {
                    eprintln!("      No sessions are running.");
                } else {
                    eprintln!("      Name one explicitly: `spot stay <name>`");
                    for s in live {
                        eprintln!("        {}", s.name);
                    }
                }
                return 1;
            }
        },
    };

    let mut s = match client::connect_at(&sock, ROLE_COMMAND, 0) {
        Ok(Some(s)) => s,
        Ok(None) => {
            eprintln!("spot: no session listening at {}", sock.display());
            return 1;
        }
        Err(e) => {
            eprintln!("spot: {e}");
            return 1;
        }
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let mut dec = Decoder::new();
    if client::read_frame(&mut s, &mut dec)
        .ok()
        .flatten()
        .is_none()
    {
        eprintln!("spot: handshake failed");
        return 1;
    }
    if s.write_all(&encode(T_DETACH, b"")).is_err() {
        eprintln!("spot: could not send detach");
        return 1;
    }
    let _ = client::read_frame(&mut s, &mut dec);
    0
}

pub fn ls(dir: &Path) -> i32 {
    let live = sessions(dir);
    if live.is_empty() {
        println!("🐕 No sessions. `spot <name>` starts one.");
        return 0;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!(
        "\x1b[1m   {:<11}{:<16}{:>8}  {:>6}  COMMAND\x1b[0m",
        "STATE", "SESSION", "PID", "UP"
    );
    for s in &live {
        let (glyph, state) = if s.state.starts_with("dead") {
            ("🪦", s.state.clone())
        } else if s.attached {
            ("🐕", "attached".to_string())
        } else {
            ("🦮", "detached".to_string())
        };
        let pid = if s.state.starts_with("dead") {
            "-".to_string()
        } else {
            s.child_pid.to_string()
        };
        println!(
            "{:<3}{:<11}{:<16}{:>8}  {:>6}  {}",
            glyph,
            state,
            s.name,
            pid,
            human_age(now.saturating_sub(s.created)),
            s.command
        );
    }
    0
}

fn human_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// `spot drop` — SIGTERM the child's process group, escalate to SIGKILL.
pub fn drop_session(dir: &Path, name: &str, force: bool, grace: Duration) -> i32 {
    if client::probe(dir, name).is_none() {
        eprintln!("spot: no session named '{name}'");
        return 1;
    }
    let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
    if !signal(dir, name, sig) {
        return 1;
    }
    if force {
        println!("🪦 Session '{name}' dropped.");
        return 0;
    }
    // Give it the grace period, then escalate. The child has descendants, and
    // the daemon signals the whole process group.
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if client::probe(dir, name).is_none() {
            println!("🪦 Session '{name}' dropped. Socket unlinked.");
            return 0;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    signal(dir, name, libc::SIGKILL);
    std::thread::sleep(Duration::from_millis(100));
    println!("🪦 Session '{name}' dropped (forced after {grace:?}).");
    0
}

fn signal(dir: &Path, name: &str, sig: libc::c_int) -> bool {
    let mut s = match client::connect(dir, name, ROLE_COMMAND, 0) {
        Ok(Some(s)) => s,
        Ok(None) => return true,
        Err(e) => {
            eprintln!("spot: {e}");
            return false;
        }
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let mut dec = Decoder::new();
    if client::read_frame(&mut s, &mut dec)
        .ok()
        .flatten()
        .is_none()
    {
        return false;
    }
    if s.write_all(&encode(T_SIGNAL, &[sig as u8])).is_err() {
        return false;
    }
    let _ = client::read_frame(&mut s, &mut dec);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_status_line() {
        let line = "dev\t100\t200\t-zsh\t1000\t1010\t1\t42\t512\tlive";
        let s = parse_status(line).unwrap();
        assert_eq!(s.name, "dev");
        assert_eq!(s.child_pid, 200);
        assert_eq!(s.command, "-zsh");
        assert!(s.attached);
        assert_eq!(s.state, "live");
    }

    #[test]
    fn rejects_a_truncated_status_line() {
        assert!(parse_status("dev\t100").is_none());
    }

    #[test]
    fn formats_ages() {
        assert_eq!(human_age(5), "5s");
        assert_eq!(human_age(300), "5m");
        assert_eq!(human_age(7200), "2h");
        assert_eq!(human_age(172800), "2d");
    }
}
