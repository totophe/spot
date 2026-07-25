//! Wire protocol: length-prefixed typed frames (RFC-0002 §3).
//!
//! Framing is not decoration. If control signals shared a channel with raw
//! keystrokes, pressing Ctrl-B (0x02) would detach your session. Here keystrokes
//! only ever live *inside* a DATA payload, and the type byte is positional, so
//! content can never be mistaken for a command.

use std::io;

pub const PROTO_VERSION: u8 = 1;
pub const MAX_PAYLOAD: usize = 65536;

// client -> daemon
pub const T_HELLO: u8 = 0x00;
pub const T_DATA: u8 = 0x01;
pub const T_RESIZE: u8 = 0x02;
pub const T_DETACH: u8 = 0x03;
pub const T_STATUS: u8 = 0x04;
pub const T_SIGNAL: u8 = 0x05;

// daemon -> client
pub const T_HELLO_OK: u8 = 0x80;
pub const T_ODATA: u8 = 0x81;
pub const T_STATUS_RESP: u8 = 0x84;
pub const T_EXIT: u8 = 0x85;
pub const T_DETACHED: u8 = 0x86;
pub const T_ERR: u8 = 0x8F;

pub const ROLE_ATTACH: u8 = 1;
pub const ROLE_COMMAND: u8 = 2;

/// HELLO flag: take over a session that is already attached.
pub const FLAG_STEAL: u8 = 1 << 0;

// DETACHED reasons
pub const REASON_REQUESTED: u8 = 0;
pub const REASON_STOLEN: u8 = 1;
pub const REASON_SHUTDOWN: u8 = 2;

/// A decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub ty: u8,
    pub payload: Vec<u8>,
}

/// Serialise a frame: `type:u8 | len:u32 BE | payload`.
pub fn encode(ty: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(ty);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Incremental decoder: feed it arbitrary byte chunks, pull whole frames out.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Returns the next complete frame, or `None` if more bytes are needed.
    /// `Err` means the peer is speaking nonsense and the connection must close.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, io::Error> {
        if self.buf.len() < 5 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if len > MAX_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame payload {len} exceeds maximum {MAX_PAYLOAD}"),
            ));
        }
        if self.buf.len() < 5 + len {
            return Ok(None);
        }
        let ty = self.buf[0];
        let payload = self.buf[5..5 + len].to_vec();
        self.buf.drain(..5 + len);
        Ok(Some(Frame { ty, payload }))
    }
}

/// HELLO payload: `version | role | flags | pid:u32 BE`.
pub fn encode_hello(role: u8, flags: u8, pid: u32) -> Vec<u8> {
    let mut p = vec![PROTO_VERSION, role, flags];
    p.extend_from_slice(&pid.to_be_bytes());
    encode(T_HELLO, &p)
}

pub struct Hello {
    pub version: u8,
    pub role: u8,
    pub flags: u8,
    pub pid: u32,
}

pub fn decode_hello(payload: &[u8]) -> Option<Hello> {
    if payload.len() < 7 {
        return None;
    }
    Some(Hello {
        version: payload[0],
        role: payload[1],
        flags: payload[2],
        pid: u32::from_be_bytes([payload[3], payload[4], payload[5], payload[6]]),
    })
}

/// RESIZE payload: `cols | rows | xpixel | ypixel`, each u16 BE.
pub fn encode_resize(cols: u16, rows: u16, xpix: u16, ypix: u16) -> Vec<u8> {
    let mut p = Vec::with_capacity(8);
    for v in [cols, rows, xpix, ypix] {
        p.extend_from_slice(&v.to_be_bytes());
    }
    encode(T_RESIZE, &p)
}

pub fn decode_resize(payload: &[u8]) -> Option<(u16, u16, u16, u16)> {
    if payload.len() < 8 {
        return None;
    }
    let g = |i: usize| u16::from_be_bytes([payload[i], payload[i + 1]]);
    Some((g(0), g(2), g(4), g(6)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_byte_value() {
        // The whole point of framing: a payload containing 0x02 (Ctrl-B, tmux's
        // prefix) must survive as data and never be read as a control signal.
        let payload: Vec<u8> = (0u8..=255).collect();
        let wire = encode(T_DATA, &payload);
        let mut d = Decoder::new();
        d.feed(&wire);
        let f = d.next_frame().unwrap().unwrap();
        assert_eq!(f.ty, T_DATA);
        assert_eq!(f.payload, payload);
        assert!(f.payload.contains(&0x02));
    }

    #[test]
    fn decodes_across_arbitrary_chunk_boundaries() {
        let wire = [
            encode(T_DATA, b"hello"),
            encode(T_DETACH, b""),
            encode(T_ODATA, b"world"),
        ]
        .concat();
        let mut d = Decoder::new();
        let mut got = Vec::new();
        // Feed one byte at a time — the worst case a socket can hand us.
        for b in &wire {
            d.feed(&[*b]);
            while let Some(f) = d.next_frame().unwrap() {
                got.push(f);
            }
        }
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].payload, b"hello");
        assert_eq!(got[1].ty, T_DETACH);
        assert_eq!(got[2].payload, b"world");
    }

    #[test]
    fn rejects_oversized_frame() {
        let mut d = Decoder::new();
        d.feed(&[T_DATA, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(d.next_frame().is_err());
    }

    #[test]
    fn empty_payload_is_a_valid_frame() {
        let mut d = Decoder::new();
        d.feed(&encode(T_STATUS, b""));
        let f = d.next_frame().unwrap().unwrap();
        assert_eq!(f.ty, T_STATUS);
        assert!(f.payload.is_empty());
    }

    #[test]
    fn hello_round_trips() {
        let wire = encode_hello(ROLE_ATTACH, FLAG_STEAL, 4242);
        let mut d = Decoder::new();
        d.feed(&wire);
        let f = d.next_frame().unwrap().unwrap();
        let h = decode_hello(&f.payload).unwrap();
        assert_eq!(h.version, PROTO_VERSION);
        assert_eq!(h.role, ROLE_ATTACH);
        assert_eq!(h.flags & FLAG_STEAL, FLAG_STEAL);
        assert_eq!(h.pid, 4242);
    }

    #[test]
    fn resize_round_trips() {
        let wire = encode_resize(203, 51, 0, 0);
        let mut d = Decoder::new();
        d.feed(&wire);
        let f = d.next_frame().unwrap().unwrap();
        assert_eq!(decode_resize(&f.payload), Some((203, 51, 0, 0)));
    }
}
