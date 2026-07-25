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
    fn zero_capacity_disables_replay() {
        let mut r = Ring::new(0);
        r.push(b"anything");
        assert_eq!(r.len(), 0);
    }
}
