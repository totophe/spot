# spot

[![CI](https://github.com/totophe/spot/actions/workflows/ci.yml/badge.svg)](https://github.com/totophe/spot/actions/workflows/ci.yml)
[![Release](https://github.com/totophe/spot/actions/workflows/release.yml/badge.svg)](https://github.com/totophe/spot/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

```text
           ______
          |  ||  |
          |  ||  |     ____  ____   ___  _____
          |==||==|    / ___||  _ \ / _ \|_   _|
          |  ||  |    \___ \| |_) | | | | | |
         /|  ||  |\    ___) |  __/| |_| | | |
        | |==||==| |  |____/|_|    \___/  |_|
        | |  ||  | |
        | |  ||  | |   Pseudo Indestructible Terminal
        | |==||==| |   "Yippee-ki-yay, sessions!"
        | |  ||  | |
       /|_|==||==|_|      |   |  ||  |   |
      |___|__||__|___|
```

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

## Usage

```
spot                     Interactive session picker
spot <name>              Attach to <name>, creating it if needed
spot fetch <name>        Attach only; error if it does not exist
spot stay [name]         Detach: unhook the client, leave the session running
spot ls                  List sessions (alias: ps)
spot drop <name>         Terminate a session's child process
```

`spot <name>` creates the session if it does not exist, and announces it —
`🐕 Spot is guarding 'dev' (PID 10423)…` — so a typo shows you the name you
actually got instead of dropping you into a look-alike shell. Use `spot fetch
<name>` when you mean "attach or fail".

The bare form is shadowed by the verbs (`ls ps stay drop fetch attach help`), so
a session named `ls` cannot be reached as `spot ls`. Nothing becomes
unreachable, though — every verb takes a name, so `spot attach ls`, `spot fetch
ls`, `spot stay ls` and `spot drop ls` all work on it. The vocabulary is fixed
by [RFC-0001](docs/RFC-0001-spot-pty.md) §3 and will not grow, which is what
keeps a future verb from silently stealing an existing session name.

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

### Knowing you are inside one

`spot` has no status bar — that is a multiplexer's job, and it would mean owning
part of your screen. Every process inside a session inherits `$SPOT_SESSION`, so
put it in your prompt:

```sh
# zsh
[[ -n $SPOT_SESSION ]] && RPROMPT="%F{cyan}🐕 $SPOT_SESSION%f $RPROMPT"
```

Worth doing early. Sessions nest deliberately — running `spot other` inside a
session gives you a session inside a session, with `stay` detaching the inner
one — and without a prompt marker the two are indistinguishable.

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
3. **Kick `SIGWINCH`** so full-screen applications repaint.

`--ring-bytes 0` turns off step 2. A `--redraw` flag to disable steps 1 and 3
individually is specified but not implemented in this alpha.

The rule that keeps this child-agnostic:

> **If the alternate screen is active at reattach, the replay is skipped.**

An app in the alternate screen is a full-screen TUI: it repaints completely on
the `SIGWINCH`, so replaying its raw paint bytes would be garbage that gets
overwritten. An app *not* in the alternate screen is a shell or a build log,
where the replay is exactly what you want and `SIGWINCH` does nothing. `spot`
decides from the mode, never from guessing what it is running.

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
  `install.sh` and `spot --update` download.

See [RELEASING.md](RELEASING.md) for the checklist.

## Prior art

`spot` is knowingly in the same genre as [dtach](https://github.com/crigler/dtach)
and [abduco](https://github.com/martanne/abduco), and owes both a debt. It
differs in three ways: detach is a command rather than a key chord (neither of
those is byte-transparent by default — both grab `Ctrl-\`), terminal modes are
tracked and restored, and it ships the login picker inherited from
[tmosh](https://github.com/totophe/tmosh), its direct ancestor.

## License

MIT — see [LICENSE](LICENSE).
