//! The attach client: a dumb byte pipe (RFC-0002 §5.4).
//!
//! It parses nothing on the `stdin` path. Every one of the 256 byte values goes
//! straight through, which is what makes Neovim, Zellij, Emacs and readline work
//! without exception lists.

use crate::paths;
use crate::proto::*;
use crate::pty;
use crate::term::{self, RawGuard};

use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

pub enum Outcome {
    /// The child exited; carry its status out to our own exit code.
    ChildExited(i32),
    Detached,
    Stolen,
    Error(String),
}

/// Connect and complete the handshake. Returns `Ok(None)` when no daemon is
/// listening (the socket is stale), having cleaned up after it.
pub fn connect(dir: &Path, name: &str, role: u8, flags: u8) -> std::io::Result<Option<UnixStream>> {
    let sock = paths::socket_path(dir, name);
    match connect_at(&sock, role, flags)? {
        Some(s) => Ok(Some(s)),
        None => {
            // Only now is it safe to unlink: a live daemon and a stale file look
            // identical to `stat`, and unlinking the wrong one orphans a session.
            paths::cleanup(dir, name);
            Ok(None)
        }
    }
}

/// Connect to an explicit socket path — the `$SPOT_SOCKET` route used by `stay`.
pub fn connect_at(sock: &Path, role: u8, flags: u8) -> std::io::Result<Option<UnixStream>> {
    let stream = match UnixStream::connect(sock) {
        Ok(s) => s,
        Err(e)
            if matches!(
                e.kind(),
                ErrorKind::ConnectionRefused | ErrorKind::NotFound | ErrorKind::AddrNotAvailable
            ) =>
        {
            return Ok(None)
        }
        Err(e) => return Err(e),
    };
    let mut s = stream;
    s.write_all(&encode_hello(role, flags, std::process::id()))?;
    Ok(Some(s))
}

/// Read exactly one frame, blocking. Used for the handshake and by the short
/// command connections.
pub fn read_frame(s: &mut UnixStream, dec: &mut Decoder) -> std::io::Result<Option<Frame>> {
    loop {
        if let Some(f) = dec.next_frame()? {
            return Ok(Some(f));
        }
        let mut buf = [0u8; 4096];
        match s.read(&mut buf) {
            Ok(0) => return Ok(None),
            Ok(n) => dec.feed(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Probe a session: returns its status line, or `None` if nothing is listening.
pub fn probe(dir: &Path, name: &str) -> Option<String> {
    let mut s = connect(dir, name, ROLE_COMMAND, 0).ok().flatten()?;
    let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
    let mut dec = Decoder::new();
    // First frame is HELLO_OK.
    let hello = read_frame(&mut s, &mut dec).ok().flatten()?;
    if hello.ty != T_HELLO_OK {
        return None;
    }
    s.write_all(&encode(T_STATUS, b"")).ok()?;
    let f = read_frame(&mut s, &mut dec).ok().flatten()?;
    if f.ty != T_STATUS_RESP {
        return None;
    }
    String::from_utf8(f.payload).ok()
}

/// Turn every complete frame currently buffered into terminal output, returning
/// an `Outcome` as soon as one of them ends the session.
fn drain_frames(dec: &mut Decoder) -> Option<Outcome> {
    loop {
        match dec.next_frame() {
            Ok(Some(f)) => match f.ty {
                T_ODATA => {
                    let _ = term::write_all(libc::STDOUT_FILENO, &f.payload);
                }
                T_EXIT => {
                    let code = if f.payload.len() >= 4 {
                        i32::from_be_bytes([f.payload[0], f.payload[1], f.payload[2], f.payload[3]])
                    } else {
                        0
                    };
                    return Some(Outcome::ChildExited(code));
                }
                T_DETACHED => {
                    return Some(if f.payload.first() == Some(&REASON_STOLEN) {
                        Outcome::Stolen
                    } else {
                        Outcome::Detached
                    })
                }
                T_ERR => {
                    return Some(Outcome::Error(
                        String::from_utf8_lossy(&f.payload).into_owned(),
                    ))
                }
                _ => {}
            },
            Ok(None) => return None,
            Err(e) => return Some(Outcome::Error(e.to_string())),
        }
    }
}

/// Attach to a running session and pipe bytes until something ends it.
pub fn attach(mut sock: UnixStream) -> Outcome {
    let mut dec = Decoder::new();
    match read_frame(&mut sock, &mut dec) {
        Ok(Some(f)) if f.ty == T_HELLO_OK => {}
        Ok(Some(f)) if f.ty == T_ERR => {
            return Outcome::Error(String::from_utf8_lossy(&f.payload).into_owned())
        }
        Ok(_) => return Outcome::Error("daemon closed during handshake".into()),
        Err(e) => return Outcome::Error(e.to_string()),
    }

    // SIGPIPE would kill us mid-write; we want the error value instead.
    term::ignore(libc::SIGPIPE);

    let pipe = match term::self_pipe() {
        Ok(p) => p,
        Err(e) => return Outcome::Error(e.to_string()),
    };
    term::SIGNAL_PIPE.store(pipe.write, std::sync::atomic::Ordering::Relaxed);
    term::trap(libc::SIGWINCH);

    // Raw mode from here on. The guard restores the terminal on every exit path
    // including a panic — which is why the release profile keeps unwinding.
    let mut guard = match RawGuard::new(libc::STDIN_FILENO) {
        Ok(g) => g,
        Err(e) => return Outcome::Error(format!("cannot set raw mode: {e}")),
    };

    let _ = sock.set_nonblocking(true);
    let _ = pty::set_nonblocking(libc::STDIN_FILENO);

    let (c, r, x, y) = term::window_size();
    let mut out: Vec<u8> = encode_resize(c, r, x, y);

    // The handshake read almost always pulls the reattach payload (mode restore
    // and replay) along with HELLO_OK, since the daemon queues them together.
    // Drain it now: with a quiet child there may be no further read event to
    // trigger decoding, and the restore would sit here unseen.
    if let Some(o) = drain_frames(&mut dec) {
        guard.restore();
        return o;
    }

    let sock_fd = sock.as_raw_fd();
    let outcome = loop {
        let mut fds = [
            libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: sock_fd,
                events: libc::POLLIN | if out.is_empty() { 0 } else { libc::POLLOUT },
                revents: 0,
            },
            libc::pollfd {
                fd: pipe.read,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 3, -1) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == ErrorKind::Interrupted {
                continue;
            }
            break Outcome::Error(e.to_string());
        }

        // Window resized: recompute and tell the daemon.
        if fds[2].revents & libc::POLLIN != 0 {
            term::drain(pipe.read);
            let (c, r, x, y) = term::window_size();
            out.extend_from_slice(&encode_resize(c, r, x, y));
        }

        if fds[0].revents & libc::POLLIN != 0 {
            let mut buf = [0u8; 8192];
            match unsafe {
                libc::read(
                    libc::STDIN_FILENO,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            } {
                n if n > 0 => out.extend_from_slice(&encode(T_DATA, &buf[..n as usize])),
                0 => {
                    // Local stdin closed — treat as a detach request.
                    out.extend_from_slice(&encode(T_DETACH, b""));
                }
                _ => {}
            }
        }

        if fds[1].revents & libc::POLLOUT != 0 && !out.is_empty() {
            match sock.write(&out) {
                Ok(0) => break Outcome::Detached,
                Ok(n) => {
                    out.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => break Outcome::Detached,
            }
        }

        if fds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let mut buf = [0u8; 65536];
            match sock.read(&mut buf) {
                Ok(0) => break Outcome::Detached,
                Ok(n) => dec.feed(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => break Outcome::Detached,
            }
            if let Some(o) = drain_frames(&mut dec) {
                break o;
            }
        }
    };

    guard.restore();
    let _ = std::io::stdout().flush();
    outcome
}
