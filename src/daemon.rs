//! The session guardian (RFC-0002 §1.3, §1.4).
//!
//! One daemon owns one PTY, one child, and one socket. It survives every client
//! that ever attaches to it, and it drains the PTY whether or not anyone is
//! listening — if it stopped draining, the kernel buffer would fill and the child
//! would block, so a detached session would silently freeze.

use crate::modes::Modes;
use crate::paths;
use crate::proto::*;
use crate::pty::{self, Pty};
use crate::ring::Ring;
use crate::term;

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Outbound queue ceiling per client. Past this the client is declared lagged:
/// we drop what is queued and resync from the ring instead of buffering without
/// bound for a client on a slow link.
const MAX_QUEUE: usize = 1024 * 1024;

/// How long a daemon whose child exited before anyone attached waits around, so
/// `spot run -- make` can still report that make failed.
const LINGER_FOR_FIRST_ATTACH: Duration = Duration::from_secs(10);

/// How long the deliberately-wrong window size stays in effect on reattach.
/// Long enough for a full-screen app to notice and repaint, short enough that
/// the reflow reads as part of the redraw rather than a glitch.
const REPAINT_NUDGE: Duration = Duration::from_millis(120);

struct Conn {
    id: u64,
    stream: UnixStream,
    dec: Decoder,
    out: Vec<u8>,
    role: u8,
    pid: u32,
    closing: bool,
    dead: bool,
}

impl Conn {
    fn queue(&mut self, ty: u8, payload: &[u8]) {
        self.out.extend_from_slice(&encode(ty, payload));
    }

    /// Output data is chunked to the protocol's payload ceiling.
    fn queue_output(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(MAX_PAYLOAD) {
            self.queue(T_ODATA, chunk);
        }
    }

    fn flush(&mut self) {
        while !self.out.is_empty() {
            match self.stream.write(&self.out) {
                Ok(0) => {
                    self.dead = true;
                    return;
                }
                Ok(n) => {
                    self.out.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.dead = true;
                    return;
                }
            }
        }
        if self.closing {
            self.dead = true;
        }
    }
}

pub struct Daemon {
    name: String,
    /// The argv the child sees; `argv[0]` may differ from the program path (a
    /// login shell is `-zsh`). This is what `spot ls` shows, matching `ps`.
    argv: Vec<String>,
    dir: PathBuf,
    pty: Pty,
    child: Child,
    child_pid: i32,
    listener: UnixListener,
    conns: Vec<Conn>,
    attached: Option<u64>,
    next_id: u64,
    modes: Modes,
    ring: Ring,
    created: u64,
    last_output: u64,
    winsize: (u16, u16, u16, u16),
    keep_on_exit: bool,
    /// `Some` once the child has been reaped.
    exit_status: Option<i32>,
    pty_eof: bool,
    shutting_down: bool,
    /// True once a client has attached and been served at least once.
    served_attach: bool,
    /// Set on attach: the next resize must force a full repaint.
    pending_repaint: bool,
    /// A deliberately-wrong window size in effect, and when to put the real one
    /// back. See the resize handling for why the wrong size has to linger.
    resize_restore: Option<(Instant, (u16, u16, u16, u16))>,
    /// The size currently set on the PTY, so we can tell a real change (which
    /// the kernel signals for us) from a no-op (which it does not).
    applied_size: (u16, u16, u16, u16),
    /// While shutting down before anyone ever attached, how long to hold on so
    /// the creating client can still collect the exit status.
    linger_until: Option<Instant>,
    sig_read: RawFd,
}

/// Entry point for `spot --daemon`. Never returns.
pub fn run(
    name: String,
    program: String,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    keep_on_exit: bool,
    ring_bytes: usize,
    winsize: (u16, u16, u16, u16),
) -> ! {
    let dir = match paths::runtime_dir() {
        Ok(d) => d,
        Err(e) => fail_early(None, &format!("runtime directory unusable: {e}")),
    };
    match start(
        &dir,
        name.clone(),
        program,
        argv,
        env,
        keep_on_exit,
        ring_bytes,
        winsize,
    ) {
        Ok(mut d) => {
            d.event_loop();
            d.cleanup();
            std::process::exit(0);
        }
        Err(e) => fail_early(Some((&dir, &name)), &e.to_string()),
    }
}

/// The daemon's stdio is `/dev/null`, so a startup failure has nowhere to go.
/// Leave it in a file the client is watching for.
fn fail_early(target: Option<(&Path, &str)>, msg: &str) -> ! {
    if let Some((dir, name)) = target {
        let _ = fs::write(paths::err_path(dir, name), msg.as_bytes());
    }
    std::process::exit(1);
}

#[allow(clippy::too_many_arguments)]
fn start(
    dir: &Path,
    name: String,
    program: String,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    keep_on_exit: bool,
    ring_bytes: usize,
    winsize: (u16, u16, u16, u16),
) -> std::io::Result<Daemon> {
    // Detach from the spawning client's session so its terminal teardown cannot
    // reach us. Already-a-leader is fine, hence the ignored error.
    unsafe { libc::setsid() };

    for sig in [
        libc::SIGHUP,
        libc::SIGINT,
        libc::SIGQUIT,
        libc::SIGPIPE,
        libc::SIGTTOU,
        libc::SIGTTIN,
    ] {
        term::ignore(sig);
    }

    let pipe = term::self_pipe()?;
    term::SIGNAL_PIPE.store(pipe.write, std::sync::atomic::Ordering::Relaxed);
    term::trap(libc::SIGCHLD);
    term::trap_term(libc::SIGTERM);

    let pty = pty::open()?;
    pty.set_winsize(winsize.0, winsize.1, winsize.2, winsize.3)?;
    pty::set_nonblocking(pty.master.as_raw_fd())?;

    let sock = paths::socket_path(dir, &name);
    let _ = fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;
    listener.set_nonblocking(true)?;
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&sock, fs::Permissions::from_mode(0o600))?;
    }

    let child = pty.spawn(&program, &argv, &env)?;
    let child_pid = child.id() as i32;
    let now = unix_now();

    let _ = fs::remove_file(paths::err_path(dir, &name));

    Ok(Daemon {
        name,
        argv,
        dir: dir.to_path_buf(),
        pty,
        child,
        child_pid,
        listener,
        conns: Vec::new(),
        attached: None,
        next_id: 1,
        modes: Modes::new(),
        ring: Ring::new(ring_bytes),
        created: now,
        last_output: now,
        winsize,
        keep_on_exit,
        exit_status: None,
        pty_eof: false,
        shutting_down: false,
        served_attach: false,
        pending_repaint: false,
        resize_restore: None,
        applied_size: winsize,
        linger_until: None,
        sig_read: pipe.read,
    })
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Daemon {
    fn event_loop(&mut self) {
        loop {
            let mut fds: Vec<libc::pollfd> = Vec::with_capacity(4 + self.conns.len());
            let poll_in = |fd: RawFd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            fds.push(poll_in(self.listener.as_raw_fd()));
            fds.push(poll_in(self.sig_read));

            let master_slot = if !self.pty_eof {
                fds.push(poll_in(self.pty.master.as_raw_fd()));
                Some(fds.len() - 1)
            } else {
                None
            };

            let conn_start = fds.len();
            for c in &self.conns {
                let mut events = libc::POLLIN;
                if !c.out.is_empty() {
                    events |= libc::POLLOUT;
                }
                fds.push(libc::pollfd {
                    fd: c.stream.as_raw_fd(),
                    events,
                    revents: 0,
                });
            }

            let mut timeout = if self.linger_until.is_some() { 100 } else { -1 };
            if let Some((at, _)) = self.resize_restore {
                let ms = at.saturating_duration_since(Instant::now()).as_millis() as i32;
                timeout = if timeout < 0 {
                    ms.max(1)
                } else {
                    timeout.min(ms.max(1))
                };
            }
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == ErrorKind::Interrupted {
                    continue;
                }
                break;
            }

            if fds[1].revents & libc::POLLIN != 0 {
                term::drain(self.sig_read);
                self.reap();
                if term::took_term() {
                    self.notify_shutdown();
                    self.terminate_child(libc::SIGTERM);
                    self.shutting_down = true;
                }
            }

            if let Some(slot) = master_slot {
                if fds[slot].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                    self.pump_pty();
                }
            }

            if let Some((at, size)) = self.resize_restore {
                if Instant::now() >= at {
                    self.resize_restore = None;
                    self.apply_winsize(size);
                }
            }

            if fds[0].revents & libc::POLLIN != 0 {
                self.accept();
            }

            // Only the connections that were in the pollfd array have results.
            // `accept()` above may have appended more; they wait for the next
            // round, where their pending POLLIN is still there.
            let polled = fds.len() - conn_start;
            for (i, c) in self.conns.iter_mut().take(polled).enumerate() {
                let r = fds[conn_start + i].revents;
                if r & libc::POLLOUT != 0 {
                    c.flush();
                }
                if r & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                    read_conn(c);
                }
            }
            // Frame dispatch is separate from reading so handlers can borrow the
            // daemon mutably.
            self.dispatch();

            for c in &mut self.conns {
                c.flush();
            }
            self.sweep();

            if self.shutting_down && self.conns.iter().all(|c| c.out.is_empty()) {
                // A child that exits in milliseconds would otherwise be gone
                // before the client that started it can attach — and its exit
                // status with it. Hold the door briefly for the first attach.
                match self.linger_until {
                    Some(deadline) if Instant::now() < deadline => {}
                    _ => break,
                }
            }
        }
    }

    /// Drain the PTY master into the ring, the mode tracker, and the client.
    fn pump_pty(&mut self) {
        let mut buf = [0u8; 65536];
        loop {
            let n = unsafe {
                libc::read(
                    self.pty.master.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n > 0 {
                let data = &buf[..n as usize];
                self.modes.feed(data);
                self.ring.push(data);
                self.last_output = unix_now();
                if let Some(id) = self.attached {
                    if let Some(c) = self.conns.iter_mut().find(|c| c.id == id) {
                        if c.out.len() + data.len() > MAX_QUEUE {
                            // Lagged: throw away the backlog and resync from the
                            // ring rather than buffer without bound.
                            c.out.clear();
                            let restore = self.modes.restore_sequence();
                            c.queue_output(&restore);
                            let ring = self.ring.contents();
                            if !self.modes.alt_screen() && !crate::ring::repaints_screen(&ring) {
                                c.queue_output(&ring);
                            }
                        } else {
                            c.queue_output(data);
                        }
                    }
                }
                continue;
            }
            if n == 0 {
                // macOS reports child exit as EOF here.
                self.pty_eof = true;
                return;
            }
            let e = std::io::Error::last_os_error();
            match e.kind() {
                ErrorKind::Interrupted => continue,
                ErrorKind::WouldBlock => return,
                _ => {
                    // Linux reports child exit as EIO on the master, *not* as
                    // EOF. Treating it as an error here would be the classic bug.
                    self.pty_eof = true;
                    return;
                }
            }
        }
    }

    fn reap(&mut self) {
        if self.exit_status.is_some() {
            return;
        }
        let mut status: libc::c_int = 0;
        let r = unsafe { libc::waitpid(self.child_pid, &mut status, libc::WNOHANG) };
        if r != self.child_pid {
            return;
        }
        // Final drain: on macOS the PTY buffer is discarded when the slave
        // closes, so anything the child wrote just before exiting is lost unless
        // we read it now. This narrows the window; it cannot close it.
        if !self.pty_eof {
            self.pump_pty();
        }
        let code = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else if libc::WIFSIGNALED(status) {
            128 + libc::WTERMSIG(status)
        } else {
            0
        };
        self.exit_status = Some(code);
        self.pty_eof = true;

        // Clear modes before announcing the exit: if the child died abruptly it
        // never disabled mouse reporting or left the alternate screen.
        let clear = self.modes.clear_sequence();
        let keep = self.keep_on_exit;
        if let Some(c) = self.attached_conn() {
            if !clear.is_empty() {
                c.queue_output(&clear);
            }
            c.queue(T_EXIT, &code.to_be_bytes());
            if !keep {
                c.closing = true;
            }
        }
        if !self.keep_on_exit {
            self.shutting_down = true;
            if !self.served_attach {
                self.linger_until = Some(Instant::now() + LINGER_FOR_FIRST_ATTACH);
            }
        }
    }

    /// The daemon itself was told to go away. Leave the attached client's
    /// terminal sane and tell it why, rather than just dropping the socket.
    fn notify_shutdown(&mut self) {
        let clear = self.modes.clear_sequence();
        if let Some(c) = self.attached_conn() {
            if !clear.is_empty() {
                c.queue_output(&clear);
            }
            c.queue(T_DETACHED, &[REASON_SHUTDOWN]);
            c.closing = true;
        }
        self.attached = None;
    }

    fn terminate_child(&mut self, sig: libc::c_int) {
        if self.exit_status.is_none() {
            unsafe { libc::kill(-self.child_pid, sig) };
            unsafe { libc::kill(self.child_pid, sig) };
        }
    }

    /// Apply a window size and ensure the child gets exactly one `SIGWINCH`.
    ///
    /// The kernel raises `SIGWINCH` itself whenever `TIOCSWINSZ` actually
    /// changes the size, so signalling unconditionally delivered *two* per
    /// resize — two full re-layouts per event, which during a window drag is
    /// visible thrashing. We only signal when the kernel will not.
    fn apply_winsize(&mut self, size: (u16, u16, u16, u16)) {
        let (c, r, x, y) = size;
        let unchanged = self.applied_size == size;
        let _ = self.pty.set_winsize(c, r, x, y);
        self.applied_size = size;
        if unchanged && self.exit_status.is_none() {
            let pg = pty::foreground_pgrp(self.pty.master.as_raw_fd(), self.child_pid);
            unsafe { libc::kill(-pg, libc::SIGWINCH) };
        }
    }

    fn attached_conn(&mut self) -> Option<&mut Conn> {
        let id = self.attached?;
        self.conns.iter_mut().find(|c| c.id == id)
    }

    fn accept(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if !peer_is_us(stream.as_raw_fd()) {
                        continue;
                    }
                    let _ = stream.set_nonblocking(true);
                    let id = self.next_id;
                    self.next_id += 1;
                    self.conns.push(Conn {
                        id,
                        stream,
                        dec: Decoder::new(),
                        out: Vec::new(),
                        role: 0,
                        pid: 0,
                        closing: false,
                        dead: false,
                    });
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }
    }

    fn dispatch(&mut self) {
        loop {
            // Pull one frame from whichever connection has one ready, then act
            // on it with the daemon fully borrowable.
            let mut found = None;
            for c in &mut self.conns {
                if c.dead || c.closing {
                    continue;
                }
                match c.dec.next_frame() {
                    Ok(Some(f)) => {
                        found = Some((c.id, f));
                        break;
                    }
                    Ok(None) => continue,
                    Err(_) => {
                        c.dead = true;
                        continue;
                    }
                }
            }
            let Some((id, frame)) = found else { return };
            self.handle(id, frame);
        }
    }

    fn handle(&mut self, id: u64, f: Frame) {
        let role = match self.conns.iter().find(|c| c.id == id) {
            Some(c) => c.role,
            None => return,
        };

        if role == 0 {
            if f.ty != T_HELLO {
                self.reply_err(id, "expected HELLO");
                return;
            }
            let Some(h) = decode_hello(&f.payload) else {
                self.reply_err(id, "malformed HELLO");
                return;
            };
            if h.version != PROTO_VERSION {
                self.reply_err(
                    id,
                    &format!(
                        "protocol mismatch: daemon speaks v{PROTO_VERSION}, client speaks v{}. \
                         Restart the session with the new binary.",
                        h.version
                    ),
                );
                return;
            }
            match h.role {
                ROLE_ATTACH => self.do_attach(id, h.pid, h.flags & FLAG_STEAL != 0),
                ROLE_COMMAND => {
                    if let Some(c) = self.conns.iter_mut().find(|c| c.id == id) {
                        c.role = ROLE_COMMAND;
                        c.pid = h.pid;
                        c.queue(T_HELLO_OK, &[PROTO_VERSION]);
                    }
                }
                _ => self.reply_err(id, "unknown role"),
            }
            return;
        }

        match f.ty {
            T_DATA if role == ROLE_ATTACH && self.attached == Some(id) => {
                if self.exit_status.is_none() {
                    let _ = term::write_all(self.pty.master.as_raw_fd(), &f.payload);
                }
            }
            T_RESIZE if role == ROLE_ATTACH => {
                if let Some((cols, rows, x, y)) = decode_resize(&f.payload) {
                    let repaint = self.pending_repaint;
                    self.pending_repaint = false;
                    self.winsize = (cols, rows, x, y);

                    // Reattaching to a full-screen app is the case that decides
                    // whether this tool is any good, and a bare SIGWINCH is not
                    // enough: zellij (and others) read the size, see no change,
                    // and do nothing — leaving you looking at a black screen.
                    // Force a genuine change so the repaint is unavoidable.
                    if repaint && self.exit_status.is_none() {
                        // The wrong size has to *linger*. Setting it and putting
                        // it back immediately is useless: an app that reads the
                        // size inside its SIGWINCH handler still sees no change,
                        // which is exactly the black screen we are fixing.
                        let nudged = if rows > 1 { rows - 1 } else { rows + 1 };
                        self.apply_winsize((cols, nudged, x, y));
                        self.resize_restore =
                            Some((Instant::now() + REPAINT_NUDGE, (cols, rows, x, y)));
                    } else {
                        self.resize_restore = None;
                        self.apply_winsize((cols, rows, x, y));
                    }
                }
            }
            T_DETACH => self.do_detach(id),
            T_STATUS => {
                let line = self.status_line();
                if let Some(c) = self.conns.iter_mut().find(|c| c.id == id) {
                    c.queue(T_STATUS_RESP, line.as_bytes());
                    c.closing = true;
                }
            }
            T_SIGNAL => {
                let signo = f.payload.first().copied().unwrap_or(libc::SIGTERM as u8);
                self.terminate_child(signo as libc::c_int);
                if self.keep_on_exit && self.exit_status.is_some() {
                    // Reaping a DEAD session.
                    self.shutting_down = true;
                }
                if let Some(c) = self.conns.iter_mut().find(|c| c.id == id) {
                    c.queue(T_STATUS_RESP, b"ok");
                    c.closing = true;
                }
            }
            _ => {}
        }
    }

    fn do_attach(&mut self, id: u64, pid: u32, steal: bool) {
        if let Some(cur) = self.attached {
            if cur != id {
                if !steal {
                    let since = self
                        .conns
                        .iter()
                        .find(|c| c.id == cur)
                        .map(|c| c.pid)
                        .unwrap_or(0);
                    self.reply_err(
                        id,
                        &format!(
                            "'{}' is already attached (client pid {since}). \
                             Use `spot fetch --steal {}` to take it over.",
                            self.name, self.name
                        ),
                    );
                    return;
                }
                if let Some(c) = self.conns.iter_mut().find(|c| c.id == cur) {
                    c.queue(T_DETACHED, &[REASON_STOLEN]);
                    c.closing = true;
                }
                self.attached = None;
            }
        }

        let restore = self.modes.restore_sequence();
        let ring = self.ring.contents();
        // Does the child own the screen? If so it repaints on resize, and the
        // right thing to send is nothing at all. Alternate screen is one signal
        // but not the only one: `top` and `htop` paint full screens without it.
        let paints = self.modes.alt_screen() || crate::ring::repaints_screen(&ring);
        let replay = if paints {
            // It will repaint completely once it hears the size changed, so
            // anything replayed here is a stale frame about to be overdrawn.
            Vec::new()
        } else {
            // A shell or line-oriented program: the replay *is* the context the
            // user left behind, and SIGWINCH will do nothing for them.
            //
            ring.clone()
        };
        let dead_status = self.exit_status;
        let keep = self.keep_on_exit;

        if let Some(c) = self.conns.iter_mut().find(|c| c.id == id) {
            c.role = ROLE_ATTACH;
            c.pid = pid;
            c.queue(T_HELLO_OK, &[PROTO_VERSION]);
            c.queue_output(&restore);
            if !replay.is_empty() {
                c.queue_output(&replay);
            }
            if let Some(code) = dead_status {
                c.queue(T_EXIT, &code.to_be_bytes());
                c.closing = true;
            }
        }
        self.served_attach = true;
        self.pending_repaint = paints;
        self.linger_until = None;
        if dead_status.is_some() && keep {
            // A DEAD session is reaped by the attach that collects its status.
            self.shutting_down = true;
        } else {
            self.attached = Some(id);
        }
    }

    fn do_detach(&mut self, requester: u64) {
        let had_client = self.attached.is_some();
        if let Some(cur) = self.attached {
            let clear = self.modes.clear_sequence();
            if let Some(c) = self.conns.iter_mut().find(|c| c.id == cur) {
                // Leave the user's terminal sane: disable every mode the child
                // turned on before we let go of it.
                if !clear.is_empty() {
                    c.queue_output(&clear);
                }
                c.queue(T_DETACHED, &[REASON_REQUESTED]);
                c.closing = true;
            }
            self.attached = None;
        }
        if let Some(c) = self.conns.iter_mut().find(|c| c.id == requester) {
            if c.role == ROLE_COMMAND {
                // Second byte tells the commander whether it actually detached
                // something, so `stay` can distinguish "done" from "it was
                // already detached" instead of exiting silently either way.
                c.queue(T_DETACHED, &[REASON_REQUESTED, u8::from(had_client)]);
                c.closing = true;
            }
        }
    }

    fn status_line(&self) -> String {
        let state = match self.exit_status {
            Some(c) => format!("dead:{c}"),
            None => "live".to_string(),
        };
        let attached_pid = self
            .attached
            .and_then(|id| self.conns.iter().find(|c| c.id == id))
            .map(|c| c.pid)
            .unwrap_or(0);
        // Tab-separated: trivially parseable, trivially debuggable with socat.
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.name,
            std::process::id(),
            self.child_pid,
            self.argv.join(" ").replace('\t', " "),
            self.created,
            self.last_output,
            if self.attached.is_some() { 1 } else { 0 },
            attached_pid,
            self.ring.len(),
            state,
        )
    }

    fn reply_err(&mut self, id: u64, msg: &str) {
        if let Some(c) = self.conns.iter_mut().find(|c| c.id == id) {
            c.queue(T_ERR, msg.as_bytes());
            c.closing = true;
        }
    }

    fn sweep(&mut self) {
        let attached = self.attached;
        self.conns.retain(|c| {
            let keep = !c.dead;
            if !keep && Some(c.id) == attached {
                // handled below
            }
            keep
        });
        if let Some(id) = attached {
            if !self.conns.iter().any(|c| c.id == id) {
                // Passive detach: the client's terminal closed, the link dropped,
                // or it was killed. The child keeps running; that is the point.
                self.attached = None;
            }
        }
    }

    fn cleanup(&mut self) {
        if self.exit_status.is_none() {
            // Shutting down with a live child (SIGTERM to the daemon): do not
            // leave an orphan holding a PTY nobody can reach.
            self.terminate_child(libc::SIGHUP);
        }
        let _ = self.child.try_wait();
        paths::cleanup(&self.dir, &self.name);
    }
}

fn read_conn(c: &mut Conn) {
    let mut buf = [0u8; 65536];
    loop {
        match c.stream.read(&mut buf) {
            Ok(0) => {
                c.dead = true;
                return;
            }
            Ok(n) => c.dec.feed(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock => return,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => {
                c.dead = true;
                return;
            }
        }
    }
}

/// Reject a peer running as another user. Directory permissions already prevent
/// this; the check is defence in depth against a misconfigured runtime dir.
///
/// The two platforms are not symmetric: Linux `SO_PEERCRED` yields pid+uid+gid,
/// macOS `LOCAL_PEERCRED` yields only uid+gid. We need the uid, which both give.
#[cfg(target_os = "linux")]
fn peer_is_us(fd: RawFd) -> bool {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    r != 0 || cred.uid == unsafe { libc::getuid() }
}

#[cfg(not(target_os = "linux"))]
fn peer_is_us(fd: RawFd) -> bool {
    let mut cred: libc::xucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::xucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            fd,
            0, // SOL_LOCAL
            libc::LOCAL_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    r != 0 || cred.cr_uid == unsafe { libc::getuid() }
}
