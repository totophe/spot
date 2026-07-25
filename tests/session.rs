//! Integration tests (RFC-0002 §13).
//!
//! These drive the real binary through a real PTY, because every interesting
//! failure in a terminal multiplexer lives in the parts a unit test cannot see:
//! controlling terminals, raw byte paths, and what survives a client dying.

use std::io;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SPOT: &str = env!("CARGO_BIN_EXE_spot");

/// Isolated runtime dir per test, so tests never see each other's sessions.
struct Env {
    dir: PathBuf,
}

impl Env {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("spot-it-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A `stay` symlink, exactly as install.sh creates it, so tests can go
        // through argv[0] dispatch rather than always spelling `spot stay`.
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(SPOT, bin.join("stay")).unwrap();
        Self { dir }
    }

    /// PATH with this env's `stay` symlink in front.
    fn path_with_stay(&self) -> String {
        format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(SPOT);
        c.env("XDG_RUNTIME_DIR", &self.dir);
        c
    }

    /// Run a non-interactive subcommand and capture stdout.
    fn run(&self, args: &[&str]) -> (String, i32) {
        let out = self.cmd().args(args).output().unwrap();
        (
            String::from_utf8_lossy(&out.stdout).into_owned()
                + &String::from_utf8_lossy(&out.stderr),
            out.status.code().unwrap_or(-1),
        )
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Best effort: reap anything still guarding a session.
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for e in entries.flatten() {
                let n = e.file_name();
                let n = n.to_string_lossy();
                if let Some(name) = n.strip_suffix(".sock") {
                    let _ = self.cmd().args(["drop", name, "--force"]).output();
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A `spot` client attached through a PTY we control, i.e. exactly what an SSH
/// session looks like to it.
struct PtyClient {
    master: RawFd,
    child: Child,
}

impl PtyClient {
    fn spawn(env: &Env, args: &[&str]) -> Self {
        Self::spawn_with(env, args, &[])
    }

    fn spawn_with(env: &Env, args: &[&str], extra_env: &[(&str, &str)]) -> Self {
        let (master, slave) = open_pty();
        let child = unsafe {
            use std::os::unix::process::CommandExt;
            let mut c = env.cmd();
            for (k, v) in extra_env {
                c.env(k, v);
            }
            c.args(args)
                .stdin(stdio_dup(slave))
                .stdout(stdio_dup(slave))
                .stderr(stdio_dup(slave))
                .pre_exec(move || {
                    // The client must have a controlling terminal or it is not
                    // interactive and will refuse to do anything useful.
                    libc::setsid();
                    libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                    Ok(())
                })
                .spawn()
                .unwrap()
        };
        unsafe { libc::close(slave) };
        Self { master, child }
    }

    fn write(&self, bytes: &[u8]) {
        let mut off = 0;
        while off < bytes.len() {
            let n = unsafe {
                libc::write(
                    self.master,
                    bytes[off..].as_ptr() as *const libc::c_void,
                    bytes.len() - off,
                )
            };
            assert!(n > 0, "write to pty failed: {}", io::Error::last_os_error());
            off += n as usize;
        }
    }

    /// Accumulate output until `pred` is satisfied or we run out of patience.
    fn read_until(&self, timeout: Duration, pred: impl Fn(&[u8]) -> bool) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        let mut acc = Vec::new();
        while Instant::now() < deadline {
            if pred(&acc) {
                return acc;
            }
            let mut pfd = libc::pollfd {
                fd: self.master,
                events: libc::POLLIN,
                revents: 0,
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            let r = unsafe { libc::poll(&mut pfd, 1, remaining.as_millis().min(200) as i32) };
            if r <= 0 {
                continue;
            }
            let mut buf = [0u8; 8192];
            let n = unsafe {
                libc::read(
                    self.master,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n <= 0 {
                break;
            }
            acc.extend_from_slice(&buf[..n as usize]);
        }
        acc
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for PtyClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        unsafe { libc::close(self.master) };
    }
}

fn open_pty() -> (RawFd, RawFd) {
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(master >= 0);
        assert_eq!(libc::grantpt(master), 0);
        assert_eq!(libc::unlockpt(master), 0);
        let name = libc::ptsname(master);
        assert!(!name.is_null());
        let slave = libc::open(name, libc::O_RDWR);
        assert!(slave >= 0);
        let ws = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        libc::ioctl(master, libc::TIOCSWINSZ as _, &ws);
        (master, slave)
    }
}

/// A `Stdio` owning its own dup of `fd`, so all three of stdin/stdout/stderr can
/// point at the same PTY slave without double-closing it.
fn stdio_dup(fd: RawFd) -> Stdio {
    use std::os::fd::FromRawFd;
    unsafe { Stdio::from_raw_fd(libc::dup(fd)) }
}

/// The state column for `name`, read from `spot ls`.
///
/// Deliberately not a substring search on the whole output: "detached" contains
/// "attached", so `.contains("attached")` is true for a detached session and a
/// test written that way silently races.
fn state_of(env: &Env, name: &str) -> Option<String> {
    for line in env.run(&["ls"]).0.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        // glyph, STATE, SESSION, ...
        if cols.len() >= 3 && cols[2] == name {
            return Some(cols[1].to_string());
        }
    }
    None
}

fn wait_state(env: &Env, name: &str, want: &str) -> bool {
    wait_for(Duration::from_secs(10), || {
        state_of(env, name).as_deref() == Some(want)
    })
}

fn wait_for(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if pred() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    false
}

// ---------------------------------------------------------------------------

#[test]
fn passes_every_byte_value_through_untouched() {
    let env = Env::new("bytes");
    // `stty raw -echo` so the PTY line discipline does not interpret anything:
    // what comes back is what spot actually carried.
    let c = PtyClient::spawn(&env, &["bytes", "--", "sh", "-c", "stty raw -echo; cat"]);
    // Let the shell get as far as `cat`.
    assert!(wait_state(&env, "bytes", "attached"));
    std::thread::sleep(Duration::from_millis(300));

    // Every byte except NUL (which a PTY in raw mode still drops) and \n, which
    // the terminal translates. 0x02 is the one that matters most: it is tmux's
    // prefix and the byte RFC-0001 wanted to use as a control signal.
    let payload: Vec<u8> = (1u8..=255).filter(|b| *b != b'\n' && *b != b'\r').collect();
    c.write(&payload);

    let got = c.read_until(Duration::from_secs(5), |acc| acc.len() >= payload.len());
    assert!(
        got.windows(payload.len()).any(|w| w == payload),
        "payload did not survive the round trip: sent {} bytes, got {} bytes",
        payload.len(),
        got.len()
    );
    assert!(payload.contains(&0x02));
}

#[test]
fn session_survives_the_client_being_killed() {
    let env = Env::new("survive");
    let mut c = PtyClient::spawn(&env, &["survive", "--", "sleep", "300"]);
    assert!(wait_state(&env, "survive", "attached"));

    // SIGKILL the client: no cleanup, no cooperation. This is the closed-laptop
    // and dead-WiFi path.
    c.kill();

    assert!(
        wait_state(&env, "survive", "detached"),
        "session should have flipped to detached, got: {}",
        env.run(&["ls"]).0
    );
}

#[test]
fn refuses_a_second_attach_but_allows_a_steal() {
    let env = Env::new("steal");
    let _first = PtyClient::spawn(&env, &["steal", "--", "sleep", "300"]);
    // Must be genuinely attached before a second client can be refused.
    assert!(wait_state(&env, "steal", "attached"));

    let second = PtyClient::spawn(&env, &["fetch", "steal"]);
    let out = second.read_until(Duration::from_secs(5), |a| {
        String::from_utf8_lossy(a).contains("already attached")
    });
    assert!(
        String::from_utf8_lossy(&out).contains("already attached"),
        "expected a busy message, got: {:?}",
        String::from_utf8_lossy(&out)
    );

    let third = PtyClient::spawn(&env, &["fetch", "steal", "--steal"]);
    // The incumbent is told it was taken over.
    let out = third.read_until(Duration::from_secs(3), |a| !a.is_empty());
    assert!(
        !String::from_utf8_lossy(&out).contains("already attached"),
        "steal should have been accepted"
    );
    assert!(wait_state(&env, "steal", "attached"));
}

#[test]
fn stay_detaches_and_the_session_keeps_running() {
    let env = Env::new("stay");
    let c = PtyClient::spawn(&env, &["stayed", "--", "sleep", "300"]);
    assert!(wait_state(&env, "stayed", "attached"));

    let (out, code) = env.run(&["stay", "stayed"]);
    assert_eq!(code, 0, "stay failed: {out}");

    let msg = c.read_until(Duration::from_secs(5), |a| {
        String::from_utf8_lossy(a).contains("Detached")
    });
    assert!(
        String::from_utf8_lossy(&msg).contains("Spot will stay"),
        "client should report the detach, got: {:?}",
        String::from_utf8_lossy(&msg)
    );
    assert!(wait_state(&env, "stayed", "detached"));
}

#[test]
fn restores_terminal_modes_on_reattach_and_clears_them_on_detach() {
    // The signature behaviour (RFC-0002 §6): a full-screen app's modes come back
    // on reattach, and a graceful detach leaves the terminal sane.
    let env = Env::new("modes");
    let c = PtyClient::spawn(
        &env,
        &[
            "modes",
            "--",
            "sh",
            "-c",
            // Alternate screen + any-motion mouse + SGR mouse: exactly the
            // combination that leaves a terminal spewing garbage.
            "printf '\\033[?1049h\\033[?1003h\\033[?1006h'; sleep 300",
        ],
    );
    assert!(wait_state(&env, "modes", "attached"));
    // Let the printf reach the daemon's mode tracker.
    c.read_until(Duration::from_secs(3), |a| {
        a.windows(8).any(|w| w == b"\x1b[?1006h")
    });

    let (_, code) = env.run(&["stay", "modes"]);
    assert_eq!(code, 0);

    // On detach, every enabled mode must be disabled — not just the first one.
    let farewell = c.read_until(Duration::from_secs(5), |a| {
        String::from_utf8_lossy(a).contains("Detached")
    });
    let s = String::from_utf8_lossy(&farewell);
    for seq in ["\x1b[?1003l", "\x1b[?1006l", "\x1b[?1049l"] {
        assert!(
            s.contains(seq),
            "detach must disable {:?}; got {:?}",
            seq.escape_debug().to_string(),
            s.escape_debug().to_string()
        );
    }

    // On reattach, they must come back.
    let again = PtyClient::spawn(&env, &["fetch", "modes"]);
    let restored = again.read_until(Duration::from_secs(5), |a| {
        a.windows(8).any(|w| w == b"\x1b[?1006h")
    });
    let r = String::from_utf8_lossy(&restored);
    for seq in ["\x1b[?1049h", "\x1b[?1003h", "\x1b[?1006h"] {
        assert!(
            r.contains(seq),
            "reattach must restore {:?}; got {:?}",
            seq.escape_debug().to_string(),
            r.escape_debug().to_string()
        );
    }
}

#[test]
fn replays_scrollback_for_a_shell_but_not_for_a_full_screen_app() {
    // The rule that makes the redraw child-agnostic: replay is the right answer
    // for a line-oriented program and the wrong one for a TUI, and spot decides
    // from the alt-screen mode rather than from knowing what it is running.
    let env = Env::new("replay");
    let c = PtyClient::spawn(
        &env,
        &["plain", "--", "sh", "-c", "echo MARKER-VISIBLE; sleep 300"],
    );
    assert!(wait_state(&env, "plain", "attached"));
    c.read_until(Duration::from_secs(3), |a| {
        String::from_utf8_lossy(a).contains("MARKER-VISIBLE")
    });
    env.run(&["stay", "plain"]);

    let again = PtyClient::spawn(&env, &["fetch", "plain"]);
    let out = again.read_until(Duration::from_secs(5), |a| {
        String::from_utf8_lossy(a).contains("MARKER-VISIBLE")
    });
    assert!(
        String::from_utf8_lossy(&out).contains("MARKER-VISIBLE"),
        "a normal-screen session must replay its context"
    );

    // Same setup, but inside the alternate screen: no replay, because the app
    // repaints on the SIGWINCH that follows.
    let alt = PtyClient::spawn(
        &env,
        &[
            "alt",
            "--",
            "sh",
            "-c",
            "printf '\\033[?1049h'; echo MARKER-HIDDEN; sleep 300",
        ],
    );
    assert!(wait_state(&env, "alt", "attached"));
    alt.read_until(Duration::from_secs(3), |a| {
        String::from_utf8_lossy(a).contains("MARKER-HIDDEN")
    });
    env.run(&["stay", "alt"]);

    let alt2 = PtyClient::spawn(&env, &["fetch", "alt"]);
    let out2 = alt2.read_until(Duration::from_secs(2), |_| false);
    assert!(
        !String::from_utf8_lossy(&out2).contains("MARKER-HIDDEN"),
        "an alt-screen session must NOT replay: {:?}",
        String::from_utf8_lossy(&out2).escape_debug().to_string()
    );
}

#[test]
fn fetch_refuses_to_invent_a_session() {
    let env = Env::new("fetch");
    let (out, code) = env.run(&["fetch", "nope"]);
    assert_eq!(code, 1);
    assert!(out.contains("no session named 'nope'"), "got: {out}");
    // ...whereas the bare form creates one.
    assert!(!out.contains("panicked"));
}

#[test]
fn rejects_names_that_would_escape_the_runtime_directory() {
    let env = Env::new("names");
    for bad in ["../escape", ".hidden", "a/b"] {
        let (out, code) = env.run(&[bad]);
        assert_eq!(code, 1, "{bad} should be rejected");
        assert!(out.contains("invalid session name"), "{bad}: {out}");
    }
}

#[test]
fn drop_terminates_the_session() {
    let env = Env::new("drop");
    let _c = PtyClient::spawn(&env, &["doomed", "--", "sleep", "300"]);
    assert!(wait_state(&env, "doomed", "attached"));
    let (out, code) = env.run(&["drop", "doomed"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        wait_for(Duration::from_secs(5), || {
            state_of(&env, "doomed").is_none()
        }),
        "session should be gone: {}",
        env.run(&["ls"]).0
    );
}

#[test]
fn keeps_exit_status_when_asked() {
    // Without --keep-on-exit a finished session simply disappears; with it, the
    // status waits for you. This is the "did my 40-minute build pass?" case.
    let env = Env::new("keep");
    let _c = PtyClient::spawn(
        &env,
        &[
            "build",
            "--keep-on-exit",
            "--",
            "sh",
            "-c",
            "sleep 1; exit 42",
        ],
    );
    assert!(wait_state(&env, "build", "attached"));
    // Walk away while it is still running — the whole point of the flag.
    env.run(&["stay", "build"]);

    assert!(
        wait_for(Duration::from_secs(10), || {
            state_of(&env, "build").as_deref() == Some("dead:42")
        }),
        "expected a dead session carrying status 42, got: {}",
        env.run(&["ls"]).0
    );

    // Attaching collects the status and reaps the session.
    let mut collector = PtyClient::spawn(&env, &["fetch", "build"]);
    assert_eq!(collector.child.wait().unwrap().code(), Some(42));
    assert!(wait_for(Duration::from_secs(5), || {
        state_of(&env, "build").is_none()
    }));
}

#[test]
fn client_exit_code_is_the_child_exit_code() {
    let env = Env::new("exitcode");
    let mut c = PtyClient::spawn(&env, &["code", "--", "sh", "-c", "exit 7"]);
    let status = c.child.wait().unwrap();
    assert_eq!(status.code(), Some(7), "child status must propagate");
}

#[test]
fn stale_sockets_are_cleaned_up_rather_than_reported() {
    let env = Env::new("stale");
    // A socket with nothing behind it is what a crash or a reboot leaves.
    let sock = env.dir.join("spot").join("ghost.sock");
    std::fs::create_dir_all(sock.parent().unwrap()).unwrap();
    let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    drop(_l);
    let (out, _) = env.run(&["ls"]);
    assert!(
        !out.contains("ghost"),
        "stale socket reported as live: {out}"
    );
    assert!(!sock.exists(), "stale socket should have been unlinked");
}

#[test]
fn help_is_reachable_from_every_spelling() {
    // Regression: `stay --help` used to be read as a session name called
    // "--help". Same for `drop --help` and `fetch --help`.
    let env = Env::new("help");
    for args in [
        vec!["--help"],
        vec!["-h"],
        vec!["help"],
        vec!["stay", "--help"],
        vec!["drop", "--help"],
        vec!["fetch", "--help"],
        vec!["ls", "--help"],
    ] {
        let (out, code) = env.run(&args);
        assert_eq!(code, 0, "{args:?} should succeed, got: {out}");
        assert!(
            out.contains("Pseudo Indestructible Terminal"),
            "{args:?} should print help, got: {out}"
        );
    }
    // ...but a `--help` meant for the child must reach the child untouched.
    let (out, _) = env.run(&["helpsess", "--", "echo", "--help"]);
    assert!(
        !out.contains("Pseudo Indestructible Terminal"),
        "--help after `--` belongs to the child, got: {out}"
    );
}

#[test]
fn version_is_reachable_from_every_spelling() {
    let env = Env::new("version");
    for flag in ["--version", "-V", "-v"] {
        let (out, code) = env.run(&[flag]);
        assert_eq!(code, 0, "{flag} should succeed, got: {out}");
        assert!(
            out.starts_with("spot v"),
            "{flag} should print the version, got: {out}"
        );
    }
}

#[test]
fn the_default_child_is_a_working_login_shell() {
    // Regression: `spot <name>` with no `-- command` is THE common path, and it
    // was broken — the login-shell dash was passed as an argument instead of
    // living in argv[0], so zsh died with "bad option: -z". Every other test
    // here passes an explicit command, so none of them caught it.
    let env = Env::new("loginsh");
    let c = PtyClient::spawn_with(&env, &["loginsh"], &[("SHELL", "/bin/sh")]);
    assert!(wait_state(&env, "loginsh", "attached"));

    c.write(b"echo ARGV0=[$0]\n");
    let out = c.read_until(Duration::from_secs(5), |a| {
        String::from_utf8_lossy(a).contains("ARGV0=[-")
    });
    let s = String::from_utf8_lossy(&out);

    assert!(
        !s.contains("bad option") && !s.contains("Usage"),
        "the shell rejected its own argv: {s}"
    );
    assert!(
        s.contains("ARGV0=[-sh]"),
        "expected a login shell whose argv[0] is '-sh', got: {s}"
    );
}

#[test]
fn stay_reports_to_whoever_typed_it() {
    // The confirmation used to come only from the attached client, so detaching
    // someone else's terminal reported to *them* and left you with silence.
    let env = Env::new("stayreport");
    let _c = PtyClient::spawn(&env, &["reported", "--", "sleep", "300"]);
    assert!(wait_state(&env, "reported", "attached"));

    let (out, code) = env.run(&["stay", "reported"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("Detached 'reported'"),
        "the commander should be told it worked, got: {out}"
    );
    assert!(wait_state(&env, "reported", "detached"));

    // Detaching an already-detached session is not a failure, but it is also
    // not the same thing, and saying nothing at all is the worst of both.
    let (out, code) = env.run(&["stay", "reported"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("already detached"),
        "expected an already-detached notice, got: {out}"
    );
}

#[test]
fn bare_stay_inside_a_session_reports_exactly_once() {
    // Via the `stay` symlink and $SPOT_SOCKET — the real in-session path, which
    // no other test exercises. The attached client prints the farewell, so the
    // command itself must stay quiet or the message would appear twice.
    let env = Env::new("stayinside");
    let c = PtyClient::spawn_with(
        &env,
        &["inside"],
        &[("SHELL", "/bin/sh"), ("PATH", &env.path_with_stay())],
    );
    assert!(wait_state(&env, "inside", "attached"));

    c.write(b"stay\n");
    let out = c.read_until(Duration::from_secs(5), |a| {
        String::from_utf8_lossy(a).contains("Spot will stay")
    });
    let s = String::from_utf8_lossy(&out);

    assert_eq!(
        s.matches("Spot will stay").count(),
        1,
        "expected exactly one farewell, got: {s}"
    );
    assert!(
        !s.contains("already detached"),
        "the in-session command must not add its own line: {s}"
    );
    assert!(wait_state(&env, "inside", "detached"));
}
