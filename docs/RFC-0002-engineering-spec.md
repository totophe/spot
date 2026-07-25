# RFC 0002: `spot` — Engineering Specification

* **Status:** Draft
* **Supersedes:** parts of [RFC-0001](RFC-0001-spot-pty.md) — see §14 for the delta list
* **Scope:** everything needed to implement `spot` without further design decisions

RFC-0001 defines *what* `spot` is: the mission, the command vocabulary, the persona.
It is the product spec and remains authoritative on all of that. This document defines
*how* it works: process model, wire protocol, filesystem layout, reattach semantics,
failure handling, and the test matrix.

---

## 0. Design principles

These are binding. When a later decision in this document looks arbitrary, it was
derived from one of these.

1. **One tool, one job.** `spot` guards a PTY across client disconnects. That is the
   whole job.
2. **Child-agnostic.** `spot` never knows or cares what it is guarding. A login shell,
   `zellij`, `tmux`, `nvim`, a 40-minute build, something that does not exist yet — all
   are the same to `spot`. No special-casing, no detection, no "if zellij then…".
3. **Byte-transparent by default.** Nothing on the `stdin` path is parsed unless the
   user opts in (§9). Every one of the 256 byte values reaches the child unmodified.
4. **No state outside the session.** No registry file, no database, no config file. The
   sockets *are* the session list. When the last session ends, `spot` leaves nothing
   behind but its own binary.
5. **Complements, does not replace.** `mosh` keeps the *link* alive; `spot` keeps the
   *session* alive; `zellij`/`tmux` multiplex *within* a session. `spot` makes mosh more
   robust the way `tmosh` did — it does not compete with any of the three.

### 0.1 Explicit non-goals

Stated so they can be pointed at rather than re-argued:

* Window/pane splitting, tabs, layouts — that is the multiplexer's job.
* Full terminal emulation and screen reconstruction — see §6 for how far we go and why
  we stop there.
* Process supervision: `spot` never restarts a dead child. The child exits, the session
  ends.
* Session resurrection across reboot. Runtime state dies with the machine.
* Output logging / typescript recording.
* Any network transport. SSH and mosh already exist.
* Configuration files. Flags and environment only.

---

## 1. Process model

### 1.1 Roles

| Role | Process | Lifetime |
|---|---|---|
| **Client** | `spot attach` / `spot fetch` / `spot <name>` | The terminal session; dies freely |
| **Daemon** | one per session | Until the child exits or `spot drop` |
| **Command** | `spot stay`, `spot ls`, `spot drop` | Milliseconds; connects, speaks, exits |

Strict 1-session : 1-daemon : 1-PTY : 1-socket, per RFC-0001 §1. A crash in one
session's daemon is invisible to every other session.

### 1.2 Session creation

The client does **not** fork the daemon inline. It re-executes itself with a hidden
subcommand, mirroring the `tmosh --self-update-bg` idiom:

```
spot --daemon <name> [--] <argv...>
```

Sequence:

1. Client acquires an exclusive `flock` on `<runtime>/<name>.lock` (created `0600`).
   Failure to acquire → another client is mid-create; wait up to 2 s for the socket to
   appear, then attach to it.
2. Client spawns `spot --daemon …` with `stdin`/`stdout`/`stderr` on `/dev/null` and
   does not wait on it.
3. Client polls for `<runtime>/<name>.sock` (20 ms interval, 5 s timeout), then
   connects as a normal client. Timeout → error, and the daemon's stderr is not
   available, so the daemon writes any fatal startup error to
   `<runtime>/<name>.err` for the client to surface.
4. Daemon releases the lock once the socket is bound and listening.

Spawning rather than forking keeps the daemon's `argv` legible in `ps` and avoids
fork-safety hazards after the client has already touched the terminal.

### 1.3 Daemon startup

```
setsid()                          # new session, no controlling terminal
ignore SIGHUP, SIGINT, SIGQUIT, SIGPIPE, SIGTTOU, SIGTTIN
signalfd/self-pipe for SIGCHLD
open PTY master (posix_openpt + grantpt + unlockpt)
bind + listen on <name>.sock  (0600, in a 0700 dir)
fork child:
    setsid()
    open slave; ioctl(TIOCSCTTY)      # slave becomes controlling terminal
    dup2 slave -> 0,1,2; close master and slave
    set env (§1.5)
    execvp(argv)
parent: close the slave fd            # see gotcha below
```

**Gotcha, spelled out because it bites everyone:** the daemon must close its copy of
the *slave* fd. If it does not, `read()` on the master never signals end-of-file when
the child exits. Having closed it, on Linux the master read returns `EIO` (not `0`)
once the child is gone — **`EIO` on the PTY master must be treated as EOF**, not as an
error. On macOS the same read returns `0`. Handle both. (Verified: this platform split
is well documented — CPython's `pty` module and Ruby both carry explicit workarounds.)

**macOS caveat:** on macOS the PTY buffer is discarded when the slave closes, so output
a short-lived child wrote just before exiting can be lost if the daemon has not read it
yet. Both Ruby (bug #20682) and Apple's own developer forums document this. Mitigation:
the daemon's poll loop drains the master before handling `SIGCHLD`, and on `SIGCHLD`
performs a final non-blocking drain until `EAGAIN`/EOF. It narrows the window; it
cannot close it. Note as a known platform limitation.

### 1.4 Daemon event loop

`poll()` over: the listener, the attached client's socket (if any), any in-flight
command connections, the PTY master, and the SIGCHLD signal fd.

* PTY master readable → read → append to ring (§7) → forward as `DATA` to the attached
  client if one exists. **The daemon drains the master unconditionally, attached or
  not.** If it stops draining, the kernel PTY buffer fills and the child blocks on
  write — a detached session would silently freeze.
* Client socket readable → decode frames (§3) → act.
* SIGCHLD → reap → §10.

### 1.5 Child environment

Injected, overwriting anything inherited:

| Variable | Value |
|---|---|
| `SPOT_SOCKET` | absolute path to this session's socket |
| `SPOT_SESSION` | session name |

Overwriting is what makes nesting correct: inside an inner `spot`, `stay` targets the
*inner* session, which is what a user typing it there means.

`TERM` is inherited from the creating client and **cannot change afterwards** — the
child's environment is fixed at `exec`. Reattaching from a terminal with a different
`TERM` may render imperfectly. `tmux` and `screen` have the same limitation; document
it, do not try to solve it.

### 1.6 Default child

`spot [name]` with no explicit command runs `$SHELL` as a **login shell** (`argv[0]`
prefixed with `-`), falling back to `/bin/sh`. Explicit form:

```
spot dev-box -- cargo watch -x test
```

Recursion is not a concern: the `--init` snippet (§11) skips when `SPOT_SOCKET` is set.

---

## 2. Filesystem layout

### 2.1 Runtime directory

In precedence order:

1. `$XDG_RUNTIME_DIR/spot/`
2. `$TMPDIR/spot-$UID/`
3. `/tmp/spot-$UID/`

Created `0700`, ownership verified on every use (refuse to proceed if it exists and is
owned by another uid, or is a symlink).

**This changes RFC-0001 §1.2**, which puts sockets in `$XDG_CONFIG_HOME/spot/sessions/`.
Two reasons: `~/.config` is for configuration, and — more practically — it survives
reboot, so every crash and every power cycle leaves a stale socket behind forever.
Runtime dirs are tmpfs and self-clean. Note macOS has no `XDG_RUNTIME_DIR`, so path 2
carries it there; `$TMPDIR` is already per-user on macOS.

### 2.2 Files per session

| File | Purpose |
|---|---|
| `<name>.sock` | the session socket, `0600` |
| `<name>.lock` | creation `flock`, held only during startup |
| `<name>.err` | fatal daemon startup message, if any; unlinked on success |

### 2.3 Session names

Validated against `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`. No `/`, no leading dot, no empty
string. This is a path component; the validation is what stops it from being a path
traversal.

Default name when none is given: a short codename (the generator already exists in
`devcon/src/codename.rs` and can be lifted verbatim), retried on collision.

### 2.4 Stale sockets

`connect()` returning `ECONNREFUSED` or `ENOENT` means no daemon is listening → unlink
and treat the session as absent. **Never unlink without attempting a connect first** —
a live daemon and a stale file are indistinguishable by `stat`, and getting this wrong
orphans a running session.

---

## 3. Wire protocol

Length-prefixed binary frames in both directions.

```
+--------+------------------+------------------+
| type   | len (u32, BE)    | payload[len]     |
| u8     |                  |                  |
+--------+------------------+------------------+
```

`len` ≤ 65536. A frame exceeding it is a protocol error: reply `ERR`, close.

### 3.1 Handshake

Client's first frame is `HELLO { u8 proto_version, u8 role }`, `role ∈ {ATTACH=1,
COMMAND=2}`. Daemon replies `HELLO_OK { u8 proto_version }` or `ERR`. Version mismatch
is a hard failure with a message naming both versions — this is the upgrade path when
a new binary meets a daemon started by the old one.

### 3.2 Client → daemon

| Type | Name | Payload |
|---|---|---|
| `0x01` | `DATA` | raw stdin bytes |
| `0x02` | `RESIZE` | `u16 cols, u16 rows, u16 xpixel, u16 ypixel` |
| `0x03` | `DETACH` | — |
| `0x04` | `STATUS` | — |
| `0x05` | `SIGNAL` | `u8 signo` |
| `0x06` | `STEAL` | — |

### 3.3 Daemon → client

| Type | Name | Payload |
|---|---|---|
| `0x81` | `DATA` | raw PTY output |
| `0x84` | `STATUS_RESP` | see §8 |
| `0x85` | `EXIT` | `i32 child exit status` |
| `0x86` | `DETACHED` | `u8 reason` (0=requested, 1=stolen, 2=daemon shutting down) |
| `0x8F` | `ERR` | UTF-8 message |

### 3.4 Why framing is not optional

RFC-0001 §2.3 specifies that `stay` "sends a 1-byte control signal (`0x02`) down
`$SPOT_SOCKET`". If that byte shared a channel with raw keystrokes, **pressing Ctrl-B
would detach your session** — and `0x02` is `tmux`'s own default prefix, so it is a key
people press on purpose. Framing removes the class of bug entirely: keystrokes only
ever occur *inside* a `DATA` payload, where a `0x02` is just a byte, and the type field
is positional so it can never be confused with content.

`DETACH` is a frame on a `COMMAND`-role connection, not a magic byte on the data path.

---

## 4. `stay` — detach

### 4.1 Resolution order

1. `spot stay <name>` → that session, by name.
2. `stay` / `spot stay` with no argument → `$SPOT_SOCKET`.
3. Neither → error listing candidate sessions. **Deliberately not** "if there is exactly
   one session, use it" — a command that detaches something different depending on how
   many sessions exist is a command you cannot trust.

Path 2 is the one that carries the intended workflow: `ssh → spot → zellij` with two
tabs, `stay` typed in either host-side tab detaches the whole `spot` session (zellij and
all its tabs included), and reconnecting drops you back into zellij exactly as it was.
This works because `zellij` inherits `SPOT_SOCKET` from `spot` and its panes inherit it
from `zellij`.

Typing `stay` inside a dev container does **not** work and is out of scope by decision:
`docker exec` does not carry host environment, and the socket path is a host path
invisible from inside the container. Use `spot stay <name>` from a host-side tab.

### 4.2 How the bare word `stay` exists

Both mechanisms, because they cover different cases:

* **`argv[0]` dispatch.** The installer creates a `stay → spot` symlink beside the
  binary. If `argv[0]` basename is `stay`, behave as `spot stay`. Five lines, and it
  works from anywhere `PATH` reaches — including `:!stay` inside an editor and inside a
  `zellij` pane.
* **Shell function** emitted by `spot --init` as a fallback for hosts where the symlink
  was not installed.

---

## 5. Attach, detach, steal

### 5.1 Concurrency

Strict 1-to-1, per RFC-0001 §1. A second `ATTACH` is refused:

```
spot: 'dev-box' is already attached (since 14:02, pid 8123).
      Use `spot fetch --steal dev-box` to take it over.
```

`STEAL` makes the daemon send `DETACHED{stolen}` to the incumbent, close it, and adopt
the newcomer. The incumbent's client restores its terminal and exits 0 with a message.

Rationale for refuse-by-default: shared attach means two terminals with different
window sizes on one PTY, which forces a smallest-common-size compromise and a
permanently wrong-looking screen for someone. Not worth it for a tool whose job is
"one session, one guard".

### 5.2 Attach-or-create vs attach-only

RFC-0001 lists `spot fetch` as "alias for `spot attach`", which makes it redundant.
Refinement that makes it earn its place:

| Form | Behaviour |
|---|---|
| `spot <name>` | attach if it exists, **create** if it does not |
| `spot fetch <name>` | attach only; error if it does not exist |

A typo in the first form costs you a stray session; a typo in the second form tells you
about it. Both are useful; which you want depends on whether you are creating.

### 5.3 Passive detach

`read()` returning 0, or `EPIPE`/`ECONNRESET` on the client socket → daemon flips to
`DETACHED`, keeps draining the PTY into the ring, keeps the child running. This is the
WiFi-drop and closed-laptop path and it requires no cooperation from anything.

### 5.4 Client terminal handling

* `cfmakeraw()` on `stdin` at attach; original `termios` restored on **every** exit
  path, including panic (guard object) and `SIGTERM`.
* The client installs **no `SIGINT` handler**. In raw mode `ISIG` is off, so Ctrl-C is
  delivered to the child as the byte `0x03` — which is the entire point of byte
  transparency.
* `SIGWINCH` handler writes one byte to a self-pipe. The main loop, not the handler,
  calls `TIOCGWINSZ` and sends the `RESIZE` frame. No work in signal handlers.
* On `RESIZE`, the daemon calls `ioctl(master, TIOCSWINSZ)` and sends `SIGWINCH` to the
  child's process group.

---

## 6. Reattach and redraw

The hardest decision in the project, and the one that most distinguishes `spot` from
`dtach`. `dtach` offers `-r none|ctrl_l|winch`; `tmux` and `mosh` solve it properly by
emulating a terminal and reconstructing the screen. Emulation is off the table — it is
several thousand lines and violates principle 0.1. Doing nothing produces corrupted
screens.

The middle path: **restore terminal *modes* without reconstructing terminal *contents*.**

### 6.1 Three layers, each independently disableable

**Layer 1 — mode tracking (default on).** The daemon scans outbound PTY bytes for a
small allowlist of mode-setting sequences and keeps the current value of each. On
reattach, it emits the current set first, before anything else.

Tracked (DECSET/DECRST `CSI ? Pm h/l` unless noted):

| Mode | Meaning | Why it matters |
|---|---|---|
| `1049`, `47`, `1047` | alternate screen | reattaching into the wrong buffer is the single worst failure |
| `1` | DECCKM cursor-key mode | arrow keys send the wrong codes; every TUI misbehaves |
| `7` | DECAWM autowrap | long lines silently truncate |
| `25` | cursor visible | invisible cursor in a shell |
| `1000/1002/1003/1006/1015` | mouse reporting | terminal spews escape garbage on every mouse move |
| `2004` | bracketed paste | pasted text executes line by line |
| `ESC =` / `ESC >` | keypad application mode | numeric keypad sends wrong codes |

Roughly fifteen modes and a small scanner. **Not tracked:** screen contents, cursor
position, SGR attributes, scroll regions, character sets, tab stops. That is a terminal
emulator, and it is not our job.

**Layer 2 — ring replay (default on, 64 KiB, `--replay-bytes N`, `0` disables).** Emit
recent output so a reattached bare shell shows the context you left.

**Layer 3 — resize kick (default on).** Re-apply the current `winsize` and send
`SIGWINCH` to the child's process group *even if the size is unchanged*. Full-screen
applications repaint; shells ignore it. (Note: this is why we send the signal
explicitly rather than relying on `TIOCSWINSZ` — an unchanged size generates no signal
of its own.)

### 6.2 The rule that makes it work

> **If alternate-screen mode is active at reattach time, skip layer 2.**

An application in the alternate screen is a full-screen TUI. It will repaint completely
on the `SIGWINCH` from layer 3, so replaying its raw paint bytes is pure garbage that
gets overwritten anyway. An application *not* in the alternate screen is a shell or a
line-oriented program, where the replay is exactly the scrollback context you want and
where `SIGWINCH` does nothing.

So the two cases resolve cleanly and neither needs to know what the child is:

| Child state at reattach | Layer 1 | Layer 2 | Layer 3 | Result |
|---|---|---|---|---|
| alt screen (nvim, zellij, less) | modes restored | **skipped** | repaint | clean full redraw |
| normal screen (shell, build log) | modes restored | replay | no-op | context preserved |

`--redraw modes,replay,winch` / `--redraw none` for anyone who disagrees.

### 6.3 Symmetric use: clear modes on detach

The mode tracker knows exactly which modes are currently active. That makes the reverse
operation free and it is arguably the more valuable half:

> **On graceful detach, the client emits the *disable* sequence for every mode the
> tracker reports as active, then restores `termios`, then exits.**

Concretely: `\e[?1003l\e[?1002l\e[?1000l\e[?1006l\e[?1015l` for mouse, `\e[?2004l` for
bracketed paste, `\e[?1049l` to leave the alternate screen, `\e[?1l` and `\e>` for
cursor-key and keypad mode, `\e[?25h` to make the cursor visible again. All of them,
not a subset — see §6.5.

This runs on `stay`, on `--escape` detach, and on being stolen. It does **not** run when
the link dies, because the client dies with it (§6.5).

### 6.4 Why this is the interesting part

`dtach` gives a blunt global choice (`-r none|ctrl_l|winch`, defaulting to `ctrl_l`) and
no mode restoration, so reattaching to a mouse-enabled or alt-screen app can leave the
terminal unusable. `abduco` is a cleaner independent reimplementation of the same idea
and documents no mode handling either. `tmux` gets it right by being a terminal
emulator. Mode restoration is the cheap 90% of the emulator's benefit — it is what makes
the terminal *usable* on reattach, as distinct from *identical*. Nothing in this genre
appears to do it, and it costs a few hundred lines. If there is one thing to get right,
it is this.

Secondary differentiator worth noting: both `dtach` and `abduco` intercept `Ctrl-\` on
`stdin` **by default** (`-E` / `-e` to change or disable). `spot` intercepts nothing
unless `--escape` is passed (§9). Command-based detach is what buys that, and full byte
transparency by default is a real distinction, not a restatement of principle 3.

### 6.5 What this does and does not fix — the "gibberish after a disconnect" case

The familiar symptom: your SSH connection drops, and afterwards moving the mouse prints
`^[[<35;80;24M`-style garbage into your local terminal, or the terminal is stuck in a
weird state until you run `reset`.

**Cause:** a remote application enabled mouse tracking (`\e[?1003h`, SGR `\e[?1006h`) or
another mode, and the link died before anything sent the disable sequence. Your *local*
terminal is still in that mode with nothing left alive to turn it off. This is a
well-known, frequently-reported problem — including specifically for remote `zellij`
sessions over SSH.

**What `spot` can fix:**

* Graceful detach (§6.3) never leaves a mode enabled. Today, a clean exit only cleans up
  if the application bothers to, and many clean up partially — a widely reported bug is
  emitting `\e[?1000l` while leaving `1002`/`1003`/`1006` on. `spot` emits the full set
  for everything it saw enabled.
* Reattach after a drop puts the terminal into a known-consistent mode state instead of
  inheriting whatever the previous client left behind.

**What `spot` cannot fix:** the abrupt-drop case itself. `spot` runs on the remote host;
when the link dies, the client dies with it and has no channel to your local terminal.
Nothing server-side can fix that — the fix has to be local (`reset`, a terminal-emulator
recovery feature, or `mosh`, whose client is local, emulates the terminal, and therefore
survives the drop and can clean up). This is worth stating plainly in the README so
`spot` is not sold as solving a problem it structurally cannot.

---

## 7. Ring buffer and flow control

* Fixed-capacity overwriting ring, default 64 KiB, `--ring-bytes`.
* The daemon drains the PTY master unconditionally (§1.4).
* Outbound-to-client writes go through a bounded queue. If the queue fills — a client
  on a slow link, a child producing output faster than the network — the daemon marks
  the client **lagged**, stops queueing, and keeps draining into the ring. When the
  socket is writable again it discards the queue and performs a §6 restore instead.
  A lagging client therefore skips forward rather than falling permanently behind, and
  memory is bounded in every case.

---

## 8. `spot ls`

The client enumerates `*.sock` in the runtime dir, opens a `COMMAND` connection to each
with a 200 ms timeout, sends `STATUS`, and unlinks any socket that refuses connection.
There is no registry file — the sockets are the registry (principle 0.4).

`STATUS_RESP` carries: name, daemon pid, child pid, child `argv[0]`, created-at,
attached (bool + since), last child output timestamp, ring bytes buffered.

Output is a table; `*` or a dog glyph marks the currently attached session per
RFC-0001 §7.3. Attached sessions must be *visible but marked* rather than hidden —
`tmosh`'s last substantive commit was "hide already-attached sessions from the picker",
which is right for the **picker** (they are not attachable) and wrong for **`ls`**
(you want to see everything, especially the one you cannot attach to).

---

## 9. Escape sequences (opt-in)

Per RFC-0001 §5, off by default. `spot attach --escape '^g' dev-box` wraps `stdin` in a
state machine watching for the trigger byte.

* Pressing the escape byte twice sends one literal copy to the child — without this,
  the byte becomes unreachable.
* Escape followed by `d` detaches; followed by anything else, both bytes pass through.
* The parser is bypassed entirely when `--escape` is absent. Not "enabled with a
  trigger that never matches" — actually bypassed, so the default path has zero
  inspection cost and zero chance of a parser bug affecting it.

---

## 10. Termination

### 10.1 Child exits — while attached

Daemon performs a final drain of the PTY master (§1.3), sends `EXIT{status}`, closes,
unlinks socket and lock, exits. The client clears modes (§6.3), restores its terminal,
and exits with the child's status.

### 10.1.1 Child exits — while detached

The case that matters: you detach a 40-minute build, come back, and the session is
simply gone. Did it succeed? With a daemon that exits immediately, that information is
destroyed.

`abduco` solves this by keeping the session's exit status and reporting it on the next
reconnection. Adopting the behaviour, but opt-in rather than default:

* Default — daemon exits, session gone. Clean, no lingering processes, principle 0.4.
* `--keep-on-exit` (settable at session creation) — daemon enters `DEAD` state, holds
  the exit status and the ring, and keeps its socket. `spot ls` shows it as
  `dead(status)`. Attaching replays the ring and prints the status; the session is then
  reaped. `spot drop` reaps it without attaching.

A `DEAD` daemon holds no PTY and no child — just a socket and a buffer. It is still a
process that outlives its usefulness if you forget about it, which is why it is opt-in.

### 10.2 `spot drop <name>`

`SIGNAL{SIGTERM}` → daemon sends `SIGTERM` to the child's **process group** (not just
the child — it has descendants), waits `--grace` (default 5 s), then `SIGKILL`s the
group. `--force` skips the grace period. Socket unlinked either way.

### 10.3 Daemon exit codes and client exit codes

| Situation | Client exit |
|---|---|
| child exited normally | child's exit code |
| child killed by signal N | `128 + N` |
| detached via `stay` | `0` |
| stolen by another client | `0` |
| session busy, no `--steal` | `1` |
| no such session (`fetch`) | `1` |
| protocol/version mismatch | `1` |

---

## 11. Login integration

Inherited wholesale from `tmosh`; this is the layer that already exists as working code.

* `spot --init` emits the rc snippet: interactive shells only, TTY only, **skip when
  `$SPOT_SOCKET` is set** (this replaces `tmosh`'s `$TMUX` check and is what prevents
  spot-in-spot). Also defines the `stay` shell function (§4.2).
* `spot` with no arguments runs the picker: attachable (detached) sessions, "+ new
  session", "shell (no spot)". <kbd>Esc</kbd>/<kbd>q</kbd>/<kbd>Ctrl-C</kbd> is always
  the escape hatch. `tmosh/src/menu.rs` ports essentially unchanged.
* `spot --update` / background throttled check: `tmosh/src/update.rs` ports unchanged
  bar the repo constant and asset prefix.

---

## 12. Dependencies and build

* `rustix` (or bare `libc`) — `openpty`, `TIOCSWINSZ`/`TIOCGWINSZ`, `TIOCSCTTY`,
  `setsid`, `flock`, `poll`, `signalfd`.
* `crossterm` — the picker only, already a `tmosh` dependency.

RFC-0001 §6 claims "zero-dependency". The accurate and equally strong claim is **"no
runtime dependencies; statically linkable against musl"**. A PTY guardian cannot be
written without libc bindings, and pretending otherwise invites a reviewer to find the
`Cargo.toml` and distrust the rest.

Release profile, CI, installer, and the tag-equals-`Cargo.toml`-version release gate all
port from `tmosh` unchanged.

### 12.1 Security

* Runtime dir `0700`, sockets `0600`, ownership and symlink checks before use.
* Verify peer credentials on accept; refuse a connection from a different uid.
  Filesystem permissions already cover this — the check is defence in depth against a
  misconfigured runtime dir. The two platforms are **not** symmetric:
  * Linux `SO_PEERCRED` → `struct ucred` with pid, uid, gid in one call.
  * macOS `LOCAL_PEERCRED` → `struct xucred` with uid and gid but **no pid**; the pid
    needs a second `getsockopt` for `LOCAL_PEERPID`.

  The uid check works on both with one call. The attached-client pid reported in §5.1's
  "already attached" message needs the extra macOS call — or have the client send its
  own pid in `HELLO`, which is simpler, portable, and adequate since it is a diagnostic
  string rather than a security decision.
* Refuse to run setuid/setgid.

---

## 13. Test plan

**Unit**

* Frame codec round-trip, including a `DATA` payload containing `0x02`, `0x00`, and a
  full 0–255 sweep.
* Mode tracker against recorded byte streams captured from real `nvim`, `less`,
  `zellij`, and a mouse-reporting app.
* Session-name validation, including `../escape` and empty.
* Semver comparison (already tested in `tmosh`).

**Integration** (child = `cat`, so behaviour is exactly checkable)

* Byte transparency: write all 256 values, assert all 256 return.
* `kill -9` the client → child still alive, session listed as detached.
* Reattach → modes restored, correct replay policy for alt-screen vs normal.
* Two clients → second refused; `--steal` → first receives `DETACHED{stolen}` and exits
  0.
* Child exits → daemon exits, socket unlinked, client propagates status.
* Child exits while detached under `--keep-on-exit` → `ls` reports `dead(status)`;
  attaching prints the status and reaps.
* Mode clearing on detach: child enables mouse `1003`+`1006`, bracketed paste and alt
  screen; `stay`; assert the client emitted the disable sequence for **every** enabled
  mode before restoring `termios`. This is the regression test for the partial-cleanup
  bug that plagues other tools (§6.5).
* Stale socket → `ls` unlinks it and does not report a session.

**Manual matrix** — the thing that actually decides whether this works. Each child ×
each detach method:

* Children: bare `zsh`, `nvim`, `zellij`, `tmux`, `less`, a mouse-mode TUI, a plain
  build log.
* Detach: `stay`, closed terminal tab, `kill -9` client, real network drop, laptop
  sleep, `spot fetch --steal` from a second terminal.
* Also: resize the window while detached, then reattach.

---

## 14. Delta against RFC-0001

RFC-0001 stays authoritative on mission, persona, command vocabulary, and the
command-based-detach decision. These specific points are superseded:

| RFC-0001 | Change | Why |
|---|---|---|
| §1.2 sockets in `$XDG_CONFIG_HOME/spot/sessions/` | → `$XDG_RUNTIME_DIR/spot/` with fallbacks (§2.1) | config dir persists across reboot → permanent stale sockets |
| §2.3 `stay` sends bare byte `0x02` | → `DETACH` frame on a `COMMAND` connection (§3.4) | Ctrl-B would otherwise detach your session |
| §2.2 "dev containers inherit `$SPOT_SOCKET`" | → **removed**; host-side layers only (§4.1) | `docker exec` carries no host env, and the socket path is invisible in the container. Decided out of scope |
| §3 `spot fetch` = "alias for attach" | → `fetch` is attach-**only**; bare `spot <name>` is attach-or-create (§5.2) | makes `fetch` non-redundant and gives typos somewhere safe to land |
| §4 "ring buffer restores visual context" | → three-layer redraw with the alt-screen rule (§6) | raw replay alone corrupts any full-screen application |
| §6 "zero-dependency" | → "no runtime dependencies, statically linkable" (§12) | accurate, and equally strong |

---

## 14.1 Implementation status and deviations (v0.1.0 alpha)

M0, M1 and M2 are implemented. Where the code departs from the spec above, the
code is right and this section records why.

| Spec | Implementation | Why |
|---|---|---|
| §3.2 `STEAL` as its own frame type | A `FLAG_STEAL` bit in the `HELLO` payload | A separate frame after `HELLO` races the daemon's busy-check. Deciding at handshake time is atomic |
| §1.2 daemon releases the creation lock | The creating **client** holds `flock` for the whole create-and-wait, then releases | Keeps locking in one process. The loser of the race re-probes and attaches to the winner's session instead of binding |
| §10.1 daemon exits as soon as the child does | Exits, **unless** no client ever attached — then it lingers up to 10 s (`LINGER_FOR_FIRST_ATTACH`) | Found by test: `spot x -- sh -c 'exit 7'` exits before the creating client can attach, so the status was lost. A bounded linger fixes it without the weight of `--keep-on-exit` |
| §6.1 `--redraw modes,replay,winch` | Not implemented. `--ring-bytes 0` disables replay; layers 1 and 3 are always on | No demonstrated need yet. The flag stays specified |
| §9 opt-in `--escape` | Not implemented | Deferred: the default path is byte-transparent, which is what matters. Nothing else depends on it |
| §8 `spot ls` shows attached sessions marked | Done — but the **picker** hides them, since they cannot be attached | As noted in §8, the two want opposite things |

Verified by the test suite (34 unit + 12 integration, all green):

* All 256 byte values survive a round trip, `0x02` included.
* `SIGKILL` on the client leaves the child running and flips the session to
  detached.
* `stay` from inside the session detaches it; the session keeps running.
* Modes are restored on reattach, and **every** enabled mode is disabled on
  detach — the partial-cleanup regression test.
* A normal-screen session replays its context; an alt-screen session does not.
* Exit status propagates, `--keep-on-exit` holds it, stale sockets are reaped.

Two bugs the tests caught that review had not:

1. The daemon's `poll` loop indexed connections added by `accept()` in the same
   iteration, walking off the end of the pollfd array. Any second connection
   crashed the daemon.
2. The client's handshake read almost always pulls the reattach payload along
   with `HELLO_OK`. Those frames sat undecoded in the buffer, so with a quiet
   child (`sleep 300`) the mode restore and replay never reached the terminal —
   reattach silently did nothing. Frames buffered during the handshake must be
   drained before entering the poll loop.

## 15. Milestones

**M0 — core.** Daemon, framing, attach/detach/steal, `stay`, `ls`, `drop`. Redraw is
`winch`-only. Done when `nvim` and `zellij` run inside `spot` with full key
transparency and survive `kill -9` of the client.

**M1 — redraw.** Mode tracker, ring replay, the alt-screen rule (§6). The manual matrix
in §13 is the acceptance test. This is where the quality of the tool is decided.

**M2 — login layer.** Port `menu.rs`, `update.rs`, `install.sh`, `ci.yml`,
`release.yml` from `tmosh`. Largely mechanical.

**M3 — packaging.** deb, release assets, `self-update` verified end to end.

### 15.1 Name collision — checked, 2026-07-25

I previously guessed that the `spot-pity` package name existed to dodge a Debian
collision. **That guess was wrong**; the results:

| Namespace | Result |
|---|---|
| Debian `/usr/bin/spot` (contents search, trixie, all arches) | **free** — no package ships it |
| Debian package named exactly `spot` | **free** — 14 packages contain "spot" (`certspotter`, `hotspot`, `spotlighter`, Spotify clients), none is `spot` |
| crates.io `spot` | **taken** — a minimal HTTP framework, v0.1.6, last published 2021-08-29. Dormant, but the name is held |

So there is no packaging reason to avoid `spot` as either the binary path or the Debian
package name. `spot-pity` is a free branding choice — and a good joke — rather than a
workaround. Keep it or drop it on taste.

The one real constraint: **`cargo publish` under the name `spot` is not available.** That
only matters if publishing to crates.io is wanted; it does not affect the binary name,
the deb, or the installer. Options if it comes up: publish as `spot-pity`, publish as
`spot-cli`, or do not publish to crates.io at all (`tmosh` and `devcon` both distribute
via GitHub Releases and neither is on crates.io).
