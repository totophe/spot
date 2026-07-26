# spot

[![CI](https://github.com/totophe/spot/actions/workflows/ci.yml/badge.svg)](https://github.com/totophe/spot/actions/workflows/ci.yml)
[![Release](https://github.com/totophe/spot/actions/workflows/release.yml/badge.svg)](https://github.com/totophe/spot/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Pseudo Indestructible Terminal** — *"Yippee-ki-yay, sessions!"*

> 🐕 **Spot sits, stays, and guards your shell.**

A single-purpose PTY session guardian. `spot` sits silently beneath your
terminal, shell, or multiplexer, keeping processes alive across network drops,
SSH timeouts and laptop sleeps — without stealing keystrokes.

**Status: alpha.** The core works and is covered by tests, but it has not yet
been lived in for a week. See [Known limits](#known-limits).

## What it is, and is not

`spot` guards a PTY. That is the whole job.

- It is **not** a multiplexer. No panes, no tabs, no layouts — run `zellij` or
  `tmux` inside it if you want those.
- It is **not** a terminal emulator. It restores terminal *modes* on reattach,
  never screen *contents* (see [Reattach](#reattach-what-actually-happens)).
- It is **child-agnostic**. A login shell, `zellij`, `nvim`, a 40-minute build —
  `spot` neither knows nor cares.

It complements the tools around it rather than replacing them: **mosh** keeps
the *link* alive, **spot** keeps the *session* alive, **zellij/tmux** multiplex
*within* a session. If you run `zellij` under `tmux` today purely for
persistence, that is two multiplexers fighting over a prefix key — `spot` has no
key bindings at all, so the multiplexer above it gets the entire keyboard.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/totophe/spot/main/install.sh | sh
```

This installs `spot` and a `stay` symlink into `~/.local/bin`. Then, to be
greeted by the session picker on every interactive login:

```sh
spot --init >> ~/.zshrc     # or ~/.bashrc
source ~/.zshrc
```

The snippet only runs in **interactive** shells, **skips** when you are already
inside a spot session, and **skips** non-TTY sessions, so `scp`, `rsync` and
scripted SSH keep working untouched.

| Variable | Default | Purpose |
|---|---|---|
| `SPOT_INSTALL_DIR` | `~/.local/bin` | where to put the binary |
| `SPOT_VERSION` | `latest` | install a specific tag, e.g. `v0.1.0` |

### Uninstall

```sh
spot self uninstall --dry-run   # show exactly what would go
spot self uninstall
```

It removes the `--init` block from your shell rc, the `stay` symlink and the
binary itself. Every rc it edits is backed up alongside as `<file>.spot-backup`
first, only the exact guarded block is touched, and a hand-mangled block with a
missing end marker is reported rather than guessed at. It refuses while sessions
are still running, since those would outlive the binary with nothing left to
reach them — `--force` if you mean it.

A tool that edits your shell rc on the way in should be able to take itself back
out.

## Usage

```
spot                     Interactive session picker
spot <name>              Attach to <name>, creating it if needed
spot fetch <name>        Attach only; error if it does not exist
spot stay [name]         Detach: unhook the client, leave the session running
spot ls                  List sessions (alias: ps)
spot where               Which session am I in, and how deep (alias: pwd)
spot drop <name>         Terminate a session's child process
spot self update         Install the latest release
spot self uninstall      Remove spot entirely (--dry-run to preview)
```

`spot <name>` creates the session if it does not exist, and announces it —
`🐕 Spot is guarding 'dev' (PID 10423)…` — so a typo shows you the name you
actually got instead of dropping you into a look-alike shell. Use `spot fetch
<name>` when you mean "attach or fail".

The bare form is shadowed by the verbs (`ls ps where pwd stay drop fetch attach
self help`), so a session named `ls` cannot be reached as `spot ls`. Nothing becomes
unreachable, though — every verb takes a name, so `spot attach ls`, `spot fetch
ls`, `spot stay ls` and `spot drop ls` all work on it.

The vocabulary is defined by [RFC-0001](docs/RFC-0001-spot-pty.md) §3, and
adding to it is a deliberate, documented change rather than a casual one —
because a new verb silently shadows any existing session of that name in the
bare form. `where`/`pwd` were added during the alpha for exactly one reason
(see below) and the bar for the next one is high.

```sh
spot dev                              # a login shell, guarded
spot build -- cargo watch -x test     # guard a specific command
spot build --keep-on-exit -- make     # ...and keep its exit status for later
spot fetch dev --steal                # take over a session attached elsewhere
```

### Detaching

`spot` uses a **command**, not a key chord. That is the whole reason it composes:
there is no prefix key to collide with zellij, tmux, Emacs or anything else you
nest inside it.

```sh
stay              # from anywhere inside the session
spot stay dev     # from outside, by name
```

`stay` finds its session through `$SPOT_SOCKET`, which every process inside
inherits. So with `ssh → spot → zellij` and two zellij tabs, typing `stay` in
either tab detaches the whole spot session — zellij and all its tabs — and
reconnecting puts you back exactly where you were.

Whoever types the command gets the confirmation: detaching your own session
prints the farewell once, and `spot stay <other>` tells *you* it worked rather
than only telling the terminal you just let go of.

Closing your terminal, losing WiFi, or shutting the laptop detaches too. The
daemon notices the broken pipe and keeps the child running.

Exiting the shell (Ctrl-D) is the *other* thing, and spot says so — `🦴 Session
'dev' ended. Spot is off duty.` Detached means still running; ended means gone.

### Knowing you are inside one

`spot` has no status bar — that is a multiplexer's job, and it would mean owning
part of your screen. Ask instead:

```console
$ spot where
🐕 Inside spot session 'dev' — `stay` detaches it.
```

Sessions nest deliberately: running `spot other` from inside one gives you a
session inside a session, and `stay` detaches the innermost. `spot where` is how
you tell which layer you are on, because `$SPOT_SESSION` only ever names the
innermost:

```console
$ spot where
🪆 3 sessions deep:

   1  outer
   2  middle
   3  inner  ← you are here

   `stay` detaches 'inner', dropping you to 'middle'.
```

It exits 0 inside a session and 1 outside, so it scripts:
`spot where >/dev/null && echo "in a session"`.

For a permanent marker, every process inside inherits `$SPOT_SESSION`
(innermost) and `$SPOT_STACK` (the full `outer:middle:inner` chain):

```sh
# zsh — put this at the END of ~/.zshrc, after your theme loads, since
# powerlevel10k and friends set RPROMPT themselves and would overwrite it.
[[ -n $SPOT_SESSION ]] && RPROMPT="%F{cyan}🐕 $SPOT_SESSION%f $RPROMPT"
```

> Typing `stay` inside a **dev container** does not work and is out of scope:
> `docker exec` does not carry host environment, and the socket path is a host
> path invisible from inside the container. Use `spot stay <name>` from a
> host-side terminal.

## Reattach: what actually happens

Reattaching is where tools in this genre usually disappoint. `spot` does three
things, in order:

1. **Restore terminal modes.** The daemon tracks about fifteen modes — alternate
   screen, cursor-key mode, mouse reporting, bracketed paste, keypad mode — and
   replays the current state. This is what makes the terminal *usable* again, as
   distinct from *identical*.
2. **Replay the ring buffer** (64 KiB by default) so a shell shows the context
   you left behind.
3. **Force a real resize** so full-screen applications repaint. A bare
   `SIGWINCH` is not enough: an app that reads the window size in its handler,
   sees no change and does nothing leaves you on a black screen — which is what
   zellij does. So on reattach spot briefly applies a size one row short, holds
   it ~120 ms, then restores the true size. The change has to *linger*, or an
   app that reads the size in-handler still sees nothing.

`--ring-bytes 0` turns off step 2. A `--redraw` flag to disable steps 1 and 3
individually is specified but not implemented in this alpha.

The rule that keeps this child-agnostic:

> **If the child paints the screen, spot sends it nothing and lets it repaint.**

A program that owns the screen redraws it once told the size changed, so
anything replayed is a stale frame it is about to overdraw — and several of
those stacked is what produces repeated footers and torn rows. A program that
paints nothing, like a shell, will never redraw itself, so its scrollback is the
only context there is and the replay is the whole point.

"Owns the screen" is not the same as "uses the alternate screen". `top` and
`htop` paint full screens without ever entering it: they home the cursor and
overdraw in place. So the signal is absolute cursor addressing — a clear, or a
home to the top-left. Line-oriented output never homes the cursor, because a
prompt that jumped to the corner of the screen would be unusable.

The question is asked of the replay buffer rather than tracked as a flag, which
makes it self-expiring: quit `top`, and once its frames age out of the buffer
the session counts as line-oriented again. `spot` never guesses what it is
running — only whether the bytes coming out of it paint.

### The garbled-terminal problem

If you have ever had an SSH connection drop and then watched your terminal spew
`^[[<35;80;24M` every time you move the mouse: that is mouse tracking left
enabled because the remote app never got to send the disable sequence.

**`spot` cannot fix that case.** It runs on the remote host; when the link dies,
the client dies with it and has no channel to your local terminal. Only a local
fix works — `reset`, your terminal's recovery command, or `mosh`, whose client
is local and cleans up after itself.

What `spot` *does* fix is the graceful half: on `stay`, it emits the disable
sequence for **every** mode the child turned on before letting go. Partial
cleanup is a common bug elsewhere — sending `\e[?1000l` while leaving `1002`,
`1003` and `1006` enabled, so the mouse keeps spewing. `spot` sends all of them.

## How it works

One session is one daemon, one PTY pair, and one Unix socket. If session `A`
crashes, session `B` never notices.

```text
┌──────────────────────────────────────────────┐
│ SSH / mosh / local terminal                  │
│   spot client — a dumb byte pipe             │
└─────────┬────────────────────────────────────┘
          │ $XDG_RUNTIME_DIR/spot/<name>.sock
┌─────────▼────────────────────────────────────┐
│ spot daemon                                  │
│   ├── PTY master (ring buffer, mode tracker) │
│   └── child: zsh, zellij, cargo, …           │
└──────────────────────────────────────────────┘
```

The client parses **nothing** on the stdin path: all 256 byte values reach the
child untouched, which is why Neovim, Zellij, Emacs and readline work without
exception lists. (An opt-in escape character is planned but not in this alpha.)

Sockets live in `$XDG_RUNTIME_DIR/spot/` — a runtime directory, not a config
directory, so a reboot cleans up after a crash instead of leaving stale sockets
forever. There is no registry file: the sockets *are* the session list, and when
the last session ends `spot` leaves nothing behind but its own binary.

## Known limits

- **Alpha.** Tested, but not yet battle-worn.
- The reattach repaint briefly resizes the terminal by one row. Full-screen apps
  reflow through it; it reads as part of the redraw.
- `TERM` is fixed when the session is created. Reattaching from a terminal with
  a different `TERM` may render imperfectly. `tmux` and `screen` share this
  limitation.
- One client at a time, by design. A second attach is refused; `--steal` takes
  over.
- `--escape` (opt-in key-chord detach, RFC-0001 §5) is not implemented yet.
- On macOS the PTY buffer is discarded when the child exits, so a very
  short-lived child's final output can be lost. Mitigated, not eliminable.
- `spot` never restarts a dead child. The child exits, the session ends.

## Design docs

- [RFC-0001](docs/RFC-0001-spot-pty.md) — mission, command vocabulary, persona.
- [RFC-0002](docs/RFC-0002-engineering-spec.md) — the engineering spec: process
  model, wire protocol, reattach semantics, test plan.

## Building from source

```sh
cargo build --release
./target/release/spot --help
cargo test --all          # 34 unit + 12 integration tests
```

The integration tests drive the real binary through a real PTY, because every
interesting failure here lives in the parts a unit test cannot see.

Dependencies: `libc`. That is the whole list.

## Releases & CI

- **CI** runs fmt, clippy (`-D warnings`), tests and a release build on every
  push and PR.
- **Release** triggers on a `v*` tag, cross-builds Linux (x86_64, aarch64) and
  macOS (aarch64), and publishes them plus `SHA256SUMS` as release assets — what
  `install.sh` and `spot self update` download.

See [RELEASING.md](RELEASING.md) for the checklist.

## Prior art

`spot` is knowingly in the same genre as [dtach](https://github.com/crigler/dtach)
and [abduco](https://github.com/martanne/abduco), and owes both a debt. It
differs in three ways: detach is a command rather than a key chord (neither of
those is byte-transparent by default — both grab `Ctrl-\`), terminal modes are
tracked and restored, and it ships the login picker inherited from
[tmosh](https://github.com/totophe/tmosh), its direct ancestor.

Against `tmux`, the comparison is a trade rather than a ranking. `tmux`
reconstructs the screen because it keeps a model of it — which means it must
understand every byte passing through, and whatever its parser does not know
(sixel and kitty graphics, newer attributes, whatever comes next) is dropped or
mangled. `spot` never interprets the stream, so it cannot corrupt it, and
equally cannot rebuild what it never stored. **`tmux` loses what it cannot
model; `spot` loses screen content it never kept.** Neither is perfect; which
one hurts depends on what you run.

## License

MIT — see [LICENSE](LICENSE).
