//! Terminal *mode* tracking (RFC-0002 §6).
//!
//! The one place `spot` looks at the byte stream. It tracks roughly a dozen
//! terminal modes — and deliberately nothing else. Screen contents, cursor
//! position, colours and scroll regions are a terminal emulator's job, and being
//! a terminal emulator is not `spot`'s job.
//!
//! Modes are what decide whether a terminal is *usable*: reattach with mouse
//! reporting stuck on and every mouse move spews escape sequences at you;
//! reattach into the wrong screen buffer and nothing makes sense. Restoring them
//! is the cheap 90% of an emulator's benefit.

/// Private modes we track, set/reset via `CSI ? <n> h` / `CSI ? <n> l`.
const TRACKED: &[u16] = &[
    1,    // DECCKM — cursor keys send application sequences
    7,    // DECAWM — autowrap
    25,   // DECTCEM — cursor visible
    47,   // legacy alternate screen
    1000, // mouse: button events
    1002, // mouse: button + drag
    1003, // mouse: any motion
    1006, // mouse: SGR encoding
    1015, // mouse: urxvt encoding
    1047, // alternate screen (no cursor save)
    1049, // alternate screen + cursor save
    2004, // bracketed paste
];

/// Modes whose default is *on* — resetting them is a change, so a fresh tracker
/// must start with them set or we would "restore" a terminal into the wrong state.
const DEFAULT_ON: &[u16] = &[7, 25];

/// Alternate-screen modes. Any one of them active means a full-screen app owns
/// the display.
const ALT_SCREEN: &[u16] = &[47, 1047, 1049];

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    /// Inside a CSI sequence; `private` records a leading `?`.
    Csi {
        private: bool,
    },
}

pub struct Modes {
    on: [bool; TRACKED.len()],
    keypad_app: bool,
    state: State,
    params: Vec<u8>,
}

impl Default for Modes {
    fn default() -> Self {
        Self::new()
    }
}

impl Modes {
    pub fn new() -> Self {
        let mut on = [false; TRACKED.len()];
        for (i, m) in TRACKED.iter().enumerate() {
            if DEFAULT_ON.contains(m) {
                on[i] = true;
            }
        }
        Self {
            on,
            keypad_app: false,
            state: State::Ground,
            params: Vec::new(),
        }
    }

    fn idx(mode: u16) -> Option<usize> {
        TRACKED.iter().position(|m| *m == mode)
    }

    pub fn is_set(&self, mode: u16) -> bool {
        Self::idx(mode).map(|i| self.on[i]).unwrap_or(false)
    }

    /// True when a full-screen application owns the display.
    pub fn alt_screen(&self) -> bool {
        ALT_SCREEN.iter().any(|m| self.is_set(*m))
    }

    /// Feed output bytes. Incremental: escape sequences may be split across any
    /// number of calls, which is exactly what a PTY read boundary does.
    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            match self.state {
                State::Ground => {
                    if b == 0x1B {
                        self.state = State::Esc;
                    }
                }
                State::Esc => match b {
                    b'[' => {
                        self.params.clear();
                        self.state = State::Csi { private: false };
                    }
                    // Keypad application / numeric mode.
                    b'=' => {
                        self.keypad_app = true;
                        self.state = State::Ground;
                    }
                    b'>' => {
                        self.keypad_app = false;
                        self.state = State::Ground;
                    }
                    // A fresh ESC restarts the sequence rather than aborting it.
                    0x1B => self.state = State::Esc,
                    _ => self.state = State::Ground,
                },
                State::Csi { private } => {
                    if self.params.is_empty() && b == b'?' {
                        self.state = State::Csi { private: true };
                        continue;
                    }
                    // Parameter and intermediate bytes.
                    if (0x20..0x40).contains(&b) {
                        // Cap the buffer: a malformed stream must not grow it
                        // without bound.
                        if self.params.len() < 64 {
                            self.params.push(b);
                        }
                        continue;
                    }
                    // Final byte (0x40..=0x7E) ends the sequence.
                    if (0x40..=0x7E).contains(&b) {
                        if private && (b == b'h' || b == b'l') {
                            self.apply(b == b'h');
                        }
                        self.params.clear();
                        self.state = State::Ground;
                    } else {
                        // Anything else (a control byte) aborts the sequence.
                        self.params.clear();
                        self.state = State::Ground;
                    }
                }
            }
        }
    }

    fn apply(&mut self, set: bool) {
        for part in self.params.split(|c| *c == b';') {
            let s = match std::str::from_utf8(part) {
                Ok(s) => s.trim(),
                Err(_) => continue,
            };
            if let Ok(n) = s.parse::<u16>() {
                if let Some(i) = Self::idx(n) {
                    self.on[i] = set;
                }
            }
        }
    }

    /// Bytes that put a fresh terminal into the state the child believes it is
    /// in. Emitted first on reattach, before any replay.
    pub fn restore_sequence(&self) -> Vec<u8> {
        let mut out = Vec::new();
        // Alternate screen first: entering it is what defines the canvas that
        // every following mode applies to.
        for m in ALT_SCREEN {
            if self.is_set(*m) {
                out.extend_from_slice(format!("\x1b[?{m}h").as_bytes());
            }
        }
        for (i, m) in TRACKED.iter().enumerate() {
            if ALT_SCREEN.contains(m) {
                continue;
            }
            let want = self.on[i];
            let default = DEFAULT_ON.contains(m);
            // Only emit where we differ from a fresh terminal's defaults.
            if want != default {
                out.extend_from_slice(
                    format!("\x1b[?{}{}", m, if want { 'h' } else { 'l' }).as_bytes(),
                );
            }
        }
        out.extend_from_slice(if self.keypad_app { b"\x1b=" } else { b"\x1b>" });
        out
    }

    /// Bytes that return the terminal to a sane state, disabling everything the
    /// child turned on. Emitted on graceful detach and on child exit.
    ///
    /// This is the half that matters most in practice. Partial cleanup is a
    /// common bug in other tools — sending `\e[?1000l` while leaving 1002, 1003
    /// and 1006 enabled, so the mouse keeps spewing. We emit the disable for
    /// every mode we saw enabled.
    pub fn clear_sequence(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, m) in TRACKED.iter().enumerate() {
            if ALT_SCREEN.contains(m) {
                continue;
            }
            let default = DEFAULT_ON.contains(m);
            if self.on[i] != default {
                out.extend_from_slice(
                    format!("\x1b[?{}{}", m, if default { 'h' } else { 'l' }).as_bytes(),
                );
            }
        }
        if self.keypad_app {
            out.extend_from_slice(b"\x1b>");
        }
        // Leave the alternate screen last so we finish on the normal buffer with
        // the user's scrollback intact.
        for m in ALT_SCREEN.iter().rev() {
            if self.is_set(*m) {
                out.extend_from_slice(format!("\x1b[?{m}l").as_bytes());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fed(bytes: &[u8]) -> Modes {
        let mut m = Modes::new();
        m.feed(bytes);
        m
    }

    #[test]
    fn tracks_mouse_and_alt_screen() {
        let m = fed(b"\x1b[?1049h\x1b[?1002h\x1b[?1006h");
        assert!(m.alt_screen());
        assert!(m.is_set(1002));
        assert!(m.is_set(1006));
        assert!(!m.is_set(1003));
    }

    #[test]
    fn handles_multi_parameter_sequences() {
        let m = fed(b"\x1b[?1000;1002;1006h");
        assert!(m.is_set(1000) && m.is_set(1002) && m.is_set(1006));
    }

    #[test]
    fn reset_clears_a_mode() {
        let m = fed(b"\x1b[?1049h\x1b[?1049l");
        assert!(!m.alt_screen());
    }

    #[test]
    fn survives_split_across_read_boundaries() {
        // A PTY read can split an escape sequence anywhere at all.
        let full = b"\x1b[?1049h\x1b[?1003h\x1b[?2004h";
        for split in 0..full.len() {
            let mut m = Modes::new();
            m.feed(&full[..split]);
            m.feed(&full[split..]);
            assert!(m.alt_screen(), "split at {split}");
            assert!(m.is_set(1003), "split at {split}");
            assert!(m.is_set(2004), "split at {split}");
        }
    }

    #[test]
    fn ignores_non_private_and_unrelated_sequences() {
        // `CSI 1 h` (no `?`) is a different, non-private mode; SGR colour must
        // not be mistaken for anything.
        let m = fed(b"\x1b[1h\x1b[31mhello\x1b[0m\x1b[2J");
        assert!(!m.is_set(1));
    }

    #[test]
    fn tracks_keypad_mode() {
        assert!(fed(b"\x1b=").keypad_app);
        assert!(!fed(b"\x1b=\x1b>").keypad_app);
    }

    #[test]
    fn default_on_modes_start_set() {
        let m = Modes::new();
        assert!(m.is_set(7), "autowrap defaults on");
        assert!(m.is_set(25), "cursor defaults visible");
        // A fresh terminal needs no correction for defaults.
        let r = m.restore_sequence();
        assert!(!r.windows(6).any(|w| w == b"\x1b[?7l"));
    }

    #[test]
    fn clear_disables_every_enabled_mouse_mode() {
        // The regression test for the partial-cleanup bug that plagues other
        // tools: all four must be disabled, not just 1000.
        let m = fed(b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h");
        let c = String::from_utf8(m.clear_sequence()).unwrap();
        for seq in ["\x1b[?1000l", "\x1b[?1002l", "\x1b[?1003l", "\x1b[?1006l"] {
            assert!(c.contains(seq), "missing {seq:?} in {c:?}");
        }
    }

    #[test]
    fn clear_leaves_alt_screen_last() {
        let m = fed(b"\x1b[?1049h\x1b[?1003h");
        let c = m.clear_sequence();
        let s = String::from_utf8(c).unwrap();
        let mouse = s.find("\x1b[?1003l").unwrap();
        let alt = s.find("\x1b[?1049l").unwrap();
        assert!(mouse < alt, "must leave alt screen last: {s:?}");
    }

    #[test]
    fn clear_is_empty_for_an_untouched_terminal() {
        assert!(Modes::new().clear_sequence().is_empty());
    }

    #[test]
    fn restore_then_clear_is_symmetric() {
        let m = fed(b"\x1b[?1049h\x1b[?1006h\x1b[?2004h\x1b=");
        let r = String::from_utf8(m.restore_sequence()).unwrap();
        assert!(r.contains("\x1b[?1049h"));
        assert!(r.contains("\x1b[?1006h"));
        assert!(r.contains("\x1b[?2004h"));
        assert!(r.contains("\x1b="));
        let c = String::from_utf8(m.clear_sequence()).unwrap();
        assert!(c.contains("\x1b[?1049l"));
        assert!(c.contains("\x1b[?1006l"));
        assert!(c.contains("\x1b[?2004l"));
        assert!(c.contains("\x1b>"));
    }

    #[test]
    fn malformed_stream_does_not_grow_params() {
        let mut m = Modes::new();
        m.feed(b"\x1b[");
        m.feed(&vec![b'1'; 10_000]);
        assert!(m.params.len() <= 64);
        // And it still recovers for the next real sequence.
        m.feed(b"h\x1b[?1049h");
        assert!(m.alt_screen());
    }
}
