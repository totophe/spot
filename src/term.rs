//! Local terminal handling for the client and the picker.

use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// Restores the terminal's original `termios` on drop.
///
/// This is why the release profile does not use `panic = "abort"`: a panic must
/// still unwind through this guard, or the user is left with an unusable
/// terminal and no idea why.
pub struct RawGuard {
    fd: RawFd,
    saved: libc::termios,
    active: bool,
}

impl RawGuard {
    /// Put `fd` into raw mode. `ISIG` is off, so Ctrl-C reaches the child as the
    /// byte 0x03 rather than raising a signal locally — that is what byte
    /// transparency means in practice.
    pub fn new(fd: RawFd) -> io::Result<Self> {
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut saved) < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            // Block until at least one byte is available; no read timeout.
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                fd,
                saved,
                active: true,
            })
        }
    }

    pub fn restore(&mut self) {
        if self.active {
            unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
            self.active = false;
        }
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// A self-pipe: signal handlers write one byte, the poll loop reads it. Doing
/// real work inside a handler is not async-signal-safe; `write(2)` is.
pub struct SelfPipe {
    pub read: RawFd,
    pub write: RawFd,
}

pub fn self_pipe() -> io::Result<SelfPipe> {
    let mut fds = [0 as RawFd; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    crate::pty::set_nonblocking(fds[0])?;
    crate::pty::set_nonblocking(fds[1])?;
    crate::pty::set_cloexec(fds[0])?;
    crate::pty::set_cloexec(fds[1])?;
    Ok(SelfPipe {
        read: fds[0],
        write: fds[1],
    })
}

pub static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn notify(_sig: libc::c_int) {
    let fd = SIGNAL_PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        let b = 1u8;
        unsafe { libc::write(fd, &b as *const u8 as *const libc::c_void, 1) };
    }
}

/// Route `sig` into the global self-pipe.
pub fn trap(sig: libc::c_int) {
    unsafe { libc::signal(sig, notify as *const () as libc::sighandler_t) };
}

static GOT_TERM: AtomicBool = AtomicBool::new(false);

extern "C" fn notify_term(_sig: libc::c_int) {
    GOT_TERM.store(true, Ordering::Relaxed);
    notify(0);
}

/// Route a termination signal into the self-pipe *and* latch a flag, so the poll
/// loop can tell "child exited" from "someone wants us gone".
pub fn trap_term(sig: libc::c_int) {
    unsafe { libc::signal(sig, notify_term as *const () as libc::sighandler_t) };
}

/// Consumes the latched termination flag.
pub fn took_term() -> bool {
    GOT_TERM.swap(false, Ordering::Relaxed)
}

pub fn ignore(sig: libc::c_int) {
    unsafe { libc::signal(sig, libc::SIG_IGN) };
}

pub fn drain(fd: RawFd) {
    let mut buf = [0u8; 64];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            break;
        }
    }
}

/// Current size of the controlling terminal, defaulting to 80x24 when stdout is
/// not a terminal (a pipe has no size, but the child still needs plausible one).
pub fn window_size() -> (u16, u16, u16, u16) {
    crate::pty::get_winsize(libc::STDIN_FILENO)
        .or_else(|_| crate::pty::get_winsize(libc::STDOUT_FILENO))
        .unwrap_or((80, 24, 0, 0))
}

pub fn is_tty(fd: RawFd) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

/// Write all of `buf` to `fd`, retrying short writes and `EINTR`.
pub fn write_all(fd: RawFd, buf: &[u8]) -> io::Result<()> {
    let mut off = 0;
    while off < buf.len() {
        let n = unsafe {
            libc::write(
                fd,
                buf[off..].as_ptr() as *const libc::c_void,
                buf.len() - off,
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        off += n as usize;
    }
    Ok(())
}
