//! PTY allocation and child spawn (RFC-0002 §1.3).

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::process::{Child, Command};

pub struct Pty {
    pub master: OwnedFd,
    slave_path: CString,
}

/// Allocate a PTY pair. The master is `CLOEXEC` so it cannot leak into the child.
pub fn open() -> io::Result<Pty> {
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if master < 0 {
            return Err(io::Error::last_os_error());
        }
        let master = OwnedFd::from_raw_fd(master);
        if libc::grantpt(master.as_raw_fd()) != 0 || libc::unlockpt(master.as_raw_fd()) != 0 {
            return Err(io::Error::last_os_error());
        }
        // `ptsname` returns static storage; safe here because we are
        // single-threaded at this point and copy it immediately.
        let name = libc::ptsname(master.as_raw_fd());
        if name.is_null() {
            return Err(io::Error::last_os_error());
        }
        let slave_path =
            CString::from_vec_unchecked(std::ffi::CStr::from_ptr(name).to_bytes().to_vec());
        set_cloexec(master.as_raw_fd())?;
        Ok(Pty { master, slave_path })
    }
}

impl Pty {
    /// Fork/exec `argv` with the slave as its controlling terminal.
    pub fn spawn(&self, argv: &[String], env: &[(String, String)]) -> io::Result<Child> {
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        for (k, v) in env {
            cmd.env(k, v);
        }

        let slave_path = self.slave_path.clone();
        // The child's argv[0] is set by the caller (a leading `-` makes a login
        // shell), so we must not let Command override it.
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || {
                // New session; drops any inherited controlling terminal.
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                let fd = libc::open(slave_path.as_ptr(), libc::O_RDWR);
                if fd < 0 {
                    return Err(io::Error::last_os_error());
                }
                // Make the slave this session's controlling terminal.
                if libc::ioctl(fd, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                for target in 0..3 {
                    if libc::dup2(fd, target) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                if fd > 2 {
                    libc::close(fd);
                }
                // The daemon ignores a pile of signals; the child must not
                // inherit those dispositions or Ctrl-C would do nothing.
                for sig in [
                    libc::SIGHUP,
                    libc::SIGINT,
                    libc::SIGQUIT,
                    libc::SIGPIPE,
                    libc::SIGTERM,
                    libc::SIGTTOU,
                    libc::SIGTTIN,
                    libc::SIGCHLD,
                ] {
                    libc::signal(sig, libc::SIG_DFL);
                }
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
                Ok(())
            });
        }
        cmd.arg0(&argv[0]);
        cmd.spawn()
    }

    pub fn set_winsize(&self, cols: u16, rows: u16, xpix: u16, ypix: u16) -> io::Result<()> {
        set_winsize(self.master.as_raw_fd(), cols, rows, xpix, ypix)
    }
}

/// Set argv[0] separately from the program path — used to prefix a `-` for a
/// login shell.
pub trait Arg0 {
    fn arg0(&mut self, a: &str) -> &mut Self;
}

impl Arg0 for Command {
    fn arg0(&mut self, a: &str) -> &mut Self {
        use std::os::unix::process::CommandExt;
        CommandExt::arg0(self, a)
    }
}

pub fn set_winsize(fd: RawFd, cols: u16, rows: u16, xpix: u16, ypix: u16) -> io::Result<()> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: xpix,
        ws_ypixel: ypix,
    };
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &ws) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn get_winsize(fd: RawFd) -> io::Result<(u16, u16, u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut ws) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((ws.ws_col, ws.ws_row, ws.ws_xpixel, ws.ws_ypixel))
}

/// The process group that should receive `SIGWINCH`: the terminal's foreground
/// group if we can read it, else the child's own group.
pub fn foreground_pgrp(master: RawFd, child: i32) -> i32 {
    let pg = unsafe { libc::tcgetpgrp(master) };
    if pg > 0 {
        pg
    } else {
        child
    }
}

pub fn set_cloexec(fd: RawFd) -> io::Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
