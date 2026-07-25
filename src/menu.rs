//! The login picker, inherited from `tmosh` (RFC-0002 §11).
//!
//! Drawn on stderr so stdout stays clean, and <kbd>Esc</kbd> is always the
//! escape hatch: if anything at all goes wrong, you get your shell.
//!
//! Written against raw ANSI rather than a TUI crate — the client already needs
//! raw-mode and terminal plumbing, so the picker costs a few dozen extra lines
//! and keeps the dependency list at exactly one crate.

use crate::cmds::Status;
use crate::term::{self, RawGuard};
use std::io::{self, Write};

pub enum Choice {
    Attach(String),
    NewSession,
    Shell,
}

enum Item {
    Session(usize),
    New,
    Shell,
}

const HEADER_LINES: u16 = 2; // title + blank
const FOOTER_LINES: u16 = 2; // blank + hint

pub fn run(sessions: &[Status], version: &str) -> io::Result<Choice> {
    let mut items: Vec<Item> = (0..sessions.len()).map(Item::Session).collect();
    items.push(Item::New);
    items.push(Item::Shell);

    let mut guard = RawGuard::new(libc::STDIN_FILENO)?;
    let mut err = io::stderr();
    let _ = write!(err, "\x1b[?25l"); // hide cursor

    let mut selected = 0usize;
    let mut first = true;
    let result = loop {
        draw(&mut err, sessions, &items, selected, version, &mut first)?;
        match read_key() {
            Key::Up => selected = selected.checked_sub(1).unwrap_or(items.len() - 1),
            Key::Down => selected = (selected + 1) % items.len(),
            Key::Escape => break Choice::Shell,
            Key::Enter => {
                break match items[selected] {
                    Item::Session(i) => Choice::Attach(sessions[i].name.clone()),
                    Item::New => Choice::NewSession,
                    Item::Shell => Choice::Shell,
                }
            }
            Key::Other => {}
            Key::Eof => break Choice::Shell,
        }
    };

    // Wipe what we drew and put the cursor back.
    let total = items.len() as u16 + HEADER_LINES + FOOTER_LINES;
    let _ = write!(err, "\r\x1b[{}A\x1b[J\x1b[?25h", total - 1);
    let _ = err.flush();
    guard.restore();
    Ok(result)
}

fn draw(
    w: &mut impl Write,
    sessions: &[Status],
    items: &[Item],
    selected: usize,
    version: &str,
    first: &mut bool,
) -> io::Result<()> {
    let total = items.len() as u16 + HEADER_LINES + FOOTER_LINES;
    if *first {
        *first = false;
    } else {
        write!(w, "\r\x1b[{}A", total - 1)?;
    }
    write!(w, "\x1b[J")?; // clear from cursor down
    write!(w, "\x1b[1;36m  🐕 spot {version}\x1b[0m\r\n\r\n")?;

    for (idx, item) in items.iter().enumerate() {
        let sel = idx == selected;
        let pointer = if sel { "›" } else { " " };
        if sel {
            write!(w, "\x1b[1;36m")?;
        }
        match item {
            Item::Session(i) => {
                let s = &sessions[*i];
                write!(w, "  {} {}  ({})\r\n", pointer, s.name, s.command)?;
            }
            Item::New => write!(w, "  {pointer} + new session\r\n")?,
            Item::Shell => write!(w, "  {pointer} shell (no spot)\r\n")?,
        }
        if sel {
            write!(w, "\x1b[0m")?;
        }
    }

    // No trailing newline: the cursor rests on the footer so the next redraw
    // moves up exactly total-1 lines to realign.
    write!(
        w,
        "\r\n\x1b[90m  ↑/↓ move · enter select · esc → shell\x1b[0m"
    )?;
    w.flush()
}

enum Key {
    Up,
    Down,
    Enter,
    Escape,
    Other,
    Eof,
}

fn read_byte_timeout(ms: libc::c_int) -> Option<u8> {
    let mut fd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut fd, 1, ms) } <= 0 {
        return None;
    }
    let mut b = 0u8;
    let n = unsafe {
        libc::read(
            libc::STDIN_FILENO,
            &mut b as *mut u8 as *mut libc::c_void,
            1,
        )
    };
    if n == 1 {
        Some(b)
    } else {
        None
    }
}

/// Blocking read of one byte. `None` means EOF or a poll error — either way the
/// picker gives up and hands you the shell.
fn read_byte() -> Option<u8> {
    read_byte_timeout(-1)
}

fn read_key() -> Key {
    let Some(b) = read_byte() else {
        return Key::Eof;
    };
    match b {
        b'\r' | b'\n' | b' ' => Key::Enter,
        b'k' => Key::Up,
        b'j' => Key::Down,
        b'q' | 0x03 => Key::Escape,
        0x1b => {
            // Distinguish a bare Esc from an arrow key. Nothing follows a bare
            // Esc, so a short wait settles it.
            match read_byte_timeout(50) {
                Some(b'[') | Some(b'O') => match read_byte_timeout(50) {
                    Some(b'A') => Key::Up,
                    Some(b'B') => Key::Down,
                    _ => Key::Other,
                },
                Some(_) => Key::Other,
                None => Key::Escape,
            }
        }
        _ => Key::Other,
    }
}

/// Prompt for an optional session name, in cooked mode, on stderr.
pub fn prompt_name() -> Option<String> {
    let mut err = io::stderr();
    let _ = write!(err, "  Session name (empty for a codename): ");
    let _ = err.flush();
    let mut line = String::new();
    io::stdin().read_line(&mut line).ok()?;
    let name = line.trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// True when a picker makes sense at all.
pub fn interactive() -> bool {
    term::is_tty(libc::STDIN_FILENO) && term::is_tty(libc::STDERR_FILENO)
}
