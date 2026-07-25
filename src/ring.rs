//! Fixed-capacity overwriting output buffer (RFC-0002 §7).
//!
//! The daemon drains the PTY master whether or not anyone is attached — if it
//! stopped, the kernel buffer would fill and the child would block on write, so a
//! detached session would silently freeze. Everything it drains lands here.

use std::collections::VecDeque;

pub struct Ring {
    buf: VecDeque<u8>,
    cap: usize,
}

impl Ring {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap.min(64 * 1024)),
            cap,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.cap == 0 {
            return;
        }
        // A single write larger than the ring: keep only its tail.
        let bytes = if bytes.len() > self.cap {
            &bytes[bytes.len() - self.cap..]
        } else {
            bytes
        };
        let overflow = (self.buf.len() + bytes.len()).saturating_sub(self.cap);
        self.buf.drain(..overflow);
        self.buf.extend(bytes);
    }

    pub fn contents(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }
}

/// Does this output come from a program that paints the whole screen?
///
/// The question decides what a reattach should send. A program that owns the
/// screen will repaint it when told the size changed, so the right thing to
/// send is *nothing* — anything we replay is a stale frame it is about to
/// overdraw, and several of them stacked is what produces repeated footers and
/// torn rows. A line-oriented program repaints nothing, so its scrollback is
/// the only context there is, and replay is the whole point.
///
/// The signal is absolute cursor addressing: clearing the screen, or homing the
/// cursor to the top-left. `top` and `htop` are the motivating case — neither
/// uses the alternate screen, and neither clears between frames; they home and
/// overdraw in place. Line-oriented output never homes the cursor, because a
/// prompt that jumped to the corner of the screen would be unusable.
///
/// Asking this of the ring rather than tracking a flag makes it self-expiring:
/// quit `top`, and once its frames have scrolled out of the ring the session
/// counts as line-oriented again.
pub fn repaints_screen(data: &[u8]) -> bool {
    const MARKS: [&[u8]; 5] = [b"\x1b[H", b"\x1b[1;1H", b"\x1b[2J", b"\x1b[3J", b"\x1bc"];
    MARKS.iter().any(|m| data.windows(m.len()).any(|w| w == *m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_most_recent_bytes() {
        let mut r = Ring::new(4);
        r.push(b"abcdef");
        assert_eq!(r.contents(), b"cdef");
    }

    #[test]
    fn accumulates_until_full_then_slides() {
        let mut r = Ring::new(4);
        r.push(b"ab");
        assert_eq!(r.contents(), b"ab");
        r.push(b"cd");
        assert_eq!(r.contents(), b"abcd");
        r.push(b"e");
        assert_eq!(r.contents(), b"bcde");
    }

    #[test]
    fn recognises_a_screen_painter() {
        // `top`/`htop`: home the cursor and overdraw, never clearing.
        assert!(repaints_screen(b"\x1b[Hframe1........\x1b[Hframe2"));
        assert!(repaints_screen(b"\x1b[2Jcleared"));
        assert!(repaints_screen(b"\x1b[1;1Hhomed"));
        assert!(repaints_screen(b"reset\x1bcafter"));
    }

    #[test]
    fn line_oriented_output_is_not_a_painter() {
        // A shell must keep its replay: nothing will repaint it.
        assert!(!repaints_screen(b"$ ls\r\nfoo bar\r\n$ "));
        // Colour, erase-to-end-of-line and relative moves are prompt staples
        // and must not be mistaken for a full-screen repaint.
        assert!(!repaints_screen(b"\x1b[31m$\x1b[0m \x1b[K\x1b[2A\x1b[3C"));
    }

    #[test]
    fn zero_capacity_disables_replay() {
        let mut r = Ring::new(0);
        r.push(b"anything");
        assert_eq!(r.len(), 0);
    }
}
