# RFC 0001: `spot` — Pseudo Indestructible Terminal

* **Status:** Draft / Approved
* **Target Binary:** `spot`
* **Distribution Package:** `spot-pity` (Debian/Ubuntu)
* **Target Architecture:** Single Rust Binary (Linux / macOS)


> **Mission Statement:**  
> A zero-dependency, single-purpose PTY session guardian. `spot` sits silently beneath your terminal, shell, or multiplexer, ensuring processes survive network drops, SSH timeouts, and laptop sleeps—without stealing keystrokes or bloat.

---

## 1. Core Architecture

`spot` uses a strict **1-to-1 model**: Each session consists of exactly one background daemon, one PTY master/slave pair, and one Unix domain socket.

```text
┌─────────────────────────────────────────────────────────────┐
│ SSH / MOSH / LOCAL TERMINAL                                 │
│                                                             │
│   spot client (Dumb byte pipe)                              │
└─────────┬───────────────────────────────────────────────────┘
          │ Unix Domain Socket (~/.config/spot/sessions/<name>.sock)
┌─────────▼───────────────────────────────────────────────────┐
│ SPOT DAEMON (Process Guardian)                              │
│                                                             │
│   ├── PTY Master (Captures I/O, buffers ring)               │
│   └── Child Process (zsh, bash, zellij, etc.)               │
└─────────────────────────────────────────────────────────────┘
```

### Key Architectural Guarantees:
1. **Failure Isolation:** If session `A` crashes or panics, session `B` remains completely unaffected.
2. **Zero Configuration:** Sockets live in `$XDG_CONFIG_HOME/spot/sessions/` (defaulting to `~/.config/spot/sessions/`). No config files required.
3. **Transparent Pass-Through:** In default mode, the client performs zero byte parsing on `stdin`, ensuring 100% compatibility with complex key combinations in Neovim, Zellij, Emacs, and Readline.

---

## 2. The `stay` Detachment Model

To eliminate shortcut collisions across nested layers (SSH → `spot` → `zellij` → `dev container`), `spot` defaults to **Command-Based Detachment** rather than key-chord interception.

### Mechanics

1. **Session Injection:** When spawning a child process, `spot` injects:
   ```bash
   SPOT_SOCKET="/home/user/.config/spot/sessions/<session_name>.sock"
   ```
2. **Environment Inheritance:** Sub-shells, `zellij` panes, and dev containers inherit `$SPOT_SOCKET`.
3. **In-Session Detach (`stay`):** Running `stay` executes `spot stay`, sending a 1-byte control signal (`0x02`) down `$SPOT_SOCKET`.
4. **Out-of-Session Detach:** Running `spot stay <session_name>` targets the socket directly from outside the session.
5. **Passive Detach:** Closing the local terminal tab or losing WiFi naturally terminates the client pipe. The daemon detects `EOF`/broken pipe and flips to `DETACHED` state while holding the child PTY open.

---

## 3. Command Surface (CLI)

`spot` exposes a minimal vocabulary built around the loyal guard dog persona:

| Command | Usage | Description |
| :--- | :--- | :--- |
| **`spot`** | `spot [name]` | Interactive login menu (if no args), or attach/create `name`. |
| **`spot stay`** | `stay` / `spot stay [name]` | Unhook the client; keep daemon & child process alive. |
| **`spot fetch`** | `spot fetch <name>` | Alias for `spot attach <name>`. |
| **`spot ls`** | `spot ls` | List active sessions, PIDs, and attach states. |
| **`spot drop`** | `spot drop <name>` | Send `SIGTERM` to the child process and delete socket. |
| **`spot where`** | `spot where` (alias `pwd`) | Which session am I in, and how deep. Exits 0 inside, 1 outside. |

---

## 4. Signal & State Handling

```text
       +───────────+
       │  ATTACHED │
       +─────┬─────+
             │  (Network Drop / 'stay' command / Socket Closure)
             ▼
       +───────────+
       │ DETACHED  │ <─── Process stays running; I/O buffered in ring
       +─────┬─────+
             │  ('spot fetch <name>')
             ▼
       +───────────+
       │ REATTACHED│ <─── Re-syncs window geometry & flushes ring buffer
       +───────────+
```

* **`SIGWINCH` (Window Resize):** When attached, the `spot` client intercepts host terminal resize signals and transmits `struct winsize` payloads across the socket. The daemon issues `ioctl(pty_fd, TIOCSWINSZ, ...)` to resize the underlying PTY seamlessly.
* **Scrollback / Ring Buffer:** The daemon maintains a circular in-memory buffer (e.g., 64KB) of recent stdout bytes to restore visual context immediately upon reattaching.

---

## 5. Opt-in Escape Sequences

For constrained environments where opening secondary panes or running `stay` is impossible, `spot` supports an opt-in escape parser via CLI flag:

```bash
spot attach --escape '^g' dev-box
```

When enabled, the client wraps `stdin` in a state machine watching for the trigger byte sequence (`Ctrl+G` by default).

---

## 6. Packaging & Distribution Specification

* **Binary Output:** `/usr/bin/spot`
* **Package Name (Debian):** `spot-pity`
* **Dependencies:** None (Statically linked Rust binary against `libc` / `musl`).

```toml
# Cargo.toml (Packaging Metadata)
[package]
name = "spot"
version = "0.1.0"
edition = "2021"

[package.metadata.deb]
name = "spot-pity"
maintainer = "Your Name <you@example.com>"
assets = [
    ["target/release/spot", "usr/bin/spot", "755"],
]
```

---

## 7. Tone, Theme, & Identity Guidelines

While the top of the README pays mandatory homage to Nakatomi Plaza and John McClane (*"Pseudo Indestructible Terminal"*), the core operational persona throughout the binary, codebase, documentation, and user interfaces is **The Loyal Guard Dog**.

```text
       🐕  "Spot sits, stays, and guards your shell."
```

### 7.1 Thematic Directives

1. **The Header Homage:** The *"Yippee-ki-yay, sessions!"* motto and the "Pseudo Indestructible Terminal" expansion are the permanent hallmark. **Amended 2026-07-25: the Nakatomi tower ASCII art is dropped.** It never stopped reading as a rocket launching into orbit rather than a building, and the joke carries fine without it. No replacement art — a README that opens on the thing itself beats one that opens on a drawing.
2. **Dog Vocabulary in CLI & Docs:** All status messages, commands, error messages, and internal documentation should embrace guard dog terminology where natural (`stay`, `fetch`, `guarding`, `leash`, `drop`).
3. **Emoji Integration:** CLI output should utilize minimal, expressive dog emojis to make session state immediately scannable at a glance. (There is no spinning-top emoji for an Inception totem — 🪆 nesting dolls carries the joke and is literally what nesting means.)

| Event | CLI / Log Output |
| :--- | :--- |
| **New Session / Attach** | `🐕 Spot is guarding 'dev-box' (PID 10423)...` |
| **In-Session Detach (`stay`)** | `🦮 Detached from 'dev-box'. Spot will stay!` |
| **Passive Disconnect** | `🐾 Connection dropped. Session preserved in background.` |
| **Re-attach (`fetch`)** | `🦴 Re-attached to 'dev-box'. Welcome back!` |
| **Session Cleanup (`drop`)** | `🪦 Session 'dev-box' dropped. Socket unlinked.` |
| **Child exited (e.g. Ctrl-D)** | `🦴 Session 'dev-box' ended. Spot is off duty.` |
| **Nested session (`where`)** | `🪆 3 sessions deep:` |

4. **Zero-Nonsense Balance:** While the theme is friendly and personality-driven, the actual PTY byte-piping logic remains hyper-efficient, silent, and reliable. Dog references exist in user-facing interactions, never blocking or slowing down raw data streams.