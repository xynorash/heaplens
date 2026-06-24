use crate::event::AllocEvent;

#[derive(Debug)]
pub enum Frame {
    Handshake { pid: u64, name: String },
    Events(Vec<AllocEvent>),
    Symbols(Vec<(u64, String)>),
}

/// Encode a HANDSHAKE frame.
/// Wire: [u32 length][0x00][u64 pid][u16 name_len][name UTF-8]
/// length = 1 + 8 + 2 + name.len()
pub fn encode_handshake(pid: u64, name: &str) -> Vec<u8> {
    let name_bytes = name.as_bytes();
    let payload_len = 1 + 8 + 2 + name_bytes.len();
    let mut buf = Vec::with_capacity(4 + payload_len);
    buf.extend_from_slice(&u32::try_from(payload_len).expect("handshake payload exceeds u32::MAX").to_le_bytes());
    buf.push(0x00);
    buf.extend_from_slice(&pid.to_le_bytes());
    buf.extend_from_slice(&u16::try_from(name_bytes.len()).expect("name exceeds 65535 bytes").to_le_bytes());
    buf.extend_from_slice(name_bytes);
    buf
}

/// Encode an EVENTS frame.
/// Wire: [u32 length][0x01][u16 count][AllocEvent × count]
/// length = 1 + 2 + count * AllocEvent::SIZE
pub fn encode_events(events: &[AllocEvent]) -> Vec<u8> {
    let payload_len = 1 + 2 + events.len() * AllocEvent::SIZE;
    let mut buf = Vec::with_capacity(4 + payload_len);
    buf.extend_from_slice(&u32::try_from(payload_len).expect("events payload exceeds u32::MAX").to_le_bytes());
    buf.push(0x01);
    buf.extend_from_slice(&u16::try_from(events.len()).expect("events count exceeds u16::MAX").to_le_bytes());
    for ev in events {
        buf.extend_from_slice(ev.as_bytes());
    }
    buf
}

/// Encode a SYMBOLS frame.
/// Wire: [u32 length][0x02][u16 count]([u64 addr][u16 name_len][name UTF-8] × count)
pub fn encode_symbols(symbols: &[(u64, &str)]) -> Vec<u8> {
    let payload_body: usize = symbols.iter().map(|(_, n)| 8 + 2 + n.len()).sum();
    let payload_len = 1 + 2 + payload_body;
    let mut buf = Vec::with_capacity(4 + payload_len);
    buf.extend_from_slice(&u32::try_from(payload_len).expect("symbols payload exceeds u32::MAX").to_le_bytes());
    buf.push(0x02);
    buf.extend_from_slice(&u16::try_from(symbols.len()).expect("symbols count exceeds u16::MAX").to_le_bytes());
    for (addr, name) in symbols {
        let name_bytes = name.as_bytes();
        buf.extend_from_slice(&addr.to_le_bytes());
        buf.extend_from_slice(&u16::try_from(name_bytes.len()).expect("symbol name exceeds 65535 bytes").to_le_bytes());
        buf.extend_from_slice(name_bytes);
    }
    buf
}

const MAX_FRAME_LEN: u32 = 8 * 1024 * 1024; // 8 MiB

// Per-type maximum payload sizes, used to reject implausible length prefixes
// during resync without waiting for the full body to arrive.
//   Handshake (0x00): 1 tag + 8 pid + 2 name_len + 65535 name  = 65546
//   Events    (0x01): 1 tag + 2 count + 65535 * AllocEvent::SIZE (≈6.6 MiB, but capped by MAX_FRAME_LEN)
//   Symbols   (0x02): 1 tag + 2 count + 65535 * (8+2+65535) (> MAX_FRAME_LEN — no extra cap needed)
const MAX_HANDSHAKE_LEN: u32 = 1 + 8 + 2 + 65535; // 65546

enum DecoderState {
    NeedLength,
    NeedBody { total: usize },
}

pub struct FrameDecoder {
    state: DecoderState,
    buf:   Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        FrameDecoder { state: DecoderState::NeedLength, buf: Vec::new() }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Returns the next complete Frame, or None if more bytes are needed.
    /// Malformed frames are skipped silently (skip-and-continue).
    /// Incomplete frames (not enough bytes yet) return None without draining.
    pub fn next(&mut self) -> Option<Frame> {
        loop {
            match self.state {
                DecoderState::NeedLength => {
                    if self.buf.len() < 5 {
                        if self.buf.len() < 4 {
                            return None;
                        }
                        let length = u32::from_le_bytes(self.buf[0..4].try_into().unwrap());
                        if length == 0 || length > MAX_FRAME_LEN {
                            self.buf.drain(0..1);
                            continue;
                        }
                        return None; // valid-looking length, no type byte yet
                    }

                    let length = u32::from_le_bytes(self.buf[0..4].try_into().unwrap());
                    let ftype  = self.buf[4];

                    if !Self::is_plausible_header(length, ftype) {
                        self.buf.drain(0..1);
                        continue;
                    }

                    // For Handshake frames, the length is uniquely determined by the
                    // name_len field (length = 11 + name_len).  If we have enough
                    // bytes buffered to read name_len (15 bytes total: 4+1+8+2), cross-
                    // check it to reject accidental matches in junk data.
                    if ftype == 0x00 && self.buf.len() >= 15 {
                        let name_len =
                            u16::from_le_bytes(self.buf[13..15].try_into().unwrap()) as u32;
                        if length != 11 + name_len {
                            self.buf.drain(0..1);
                            continue;
                        }
                    }

                    let total = 4 + length as usize;
                    self.state = DecoderState::NeedBody { total };
                }
                DecoderState::NeedBody { total } => {
                    if self.buf.len() < total {
                        return None; // incomplete — not an error
                    }
                    let frame_bytes: Vec<u8> = self.buf.drain(0..total).collect();
                    self.state = DecoderState::NeedLength;

                    let ftype   = frame_bytes[4];
                    let payload = &frame_bytes[5..];

                    match Self::decode_payload(ftype, payload) {
                        Some(frame) => return Some(frame),
                        None => {
                            // Payload failed to decode despite valid-looking header —
                            // resync from byte 1 of what we thought was the frame start.
                            let mut prepend = frame_bytes[1..].to_vec();
                            prepend.extend_from_slice(&self.buf);
                            self.buf = prepend;
                            continue;
                        }
                    }
                }
            }
        }
    }

    /// Returns true if (length, ftype) looks like a real frame header.
    fn is_plausible_header(length: u32, ftype: u8) -> bool {
        if length == 0 || length > MAX_FRAME_LEN {
            return false;
        }
        match ftype {
            // Handshake: length = 1(tag) + 8(pid) + 2(name_len) + name_len
            // Minimum length with no payload tag (the tag is counted differently)...
            // Actually wire layout: length field does NOT include itself (4 bytes).
            // The 4-byte length field covers the entire rest: type byte + payload.
            // Handshake payload after type byte: pid(8) + name_len_field(2) + name
            // So length = 1 + 8 + 2 + name_len = 11 + name_len, range [11, 65546].
            0x00 => length >= 11 && length <= MAX_HANDSHAKE_LEN,
            // Events: length = 1(type) + 2(count) + count * SIZE = 3 + count * SIZE
            // Valid lengths: 3, 3+SIZE, 3+2*SIZE, ...
            0x01 => {
                length >= 3
                    && (length - 3) % (AllocEvent::SIZE as u32) == 0
            }
            // Symbols: length = 1 + 2 + variable, minimum 3 bytes
            0x02 => length >= 3,
            _ => false,
        }
    }

    fn decode_payload(ftype: u8, payload: &[u8]) -> Option<Frame> {
        match ftype {
            0x00 => Self::decode_handshake(payload),
            0x01 => Self::decode_events(payload),
            0x02 => Self::decode_symbols(payload),
            _    => None,
        }
    }

    fn decode_handshake(payload: &[u8]) -> Option<Frame> {
        // payload: [u64 pid][u16 name_len][name UTF-8]
        if payload.len() < 10 {
            return None;
        }
        let pid      = u64::from_le_bytes(payload[0..8].try_into().unwrap());
        let name_len = u16::from_le_bytes(payload[8..10].try_into().unwrap()) as usize;
        if payload.len() != 10 + name_len {
            return None;
        }
        let name = std::str::from_utf8(&payload[10..10 + name_len]).ok()?.to_owned();
        Some(Frame::Handshake { pid, name })
    }

    fn decode_events(payload: &[u8]) -> Option<Frame> {
        // payload: [u16 count][AllocEvent × count]
        if payload.len() < 2 {
            return None;
        }
        let count = u16::from_le_bytes(payload[0..2].try_into().unwrap()) as usize;
        if payload.len() != 2 + count * AllocEvent::SIZE {
            return None;
        }
        let mut events = Vec::with_capacity(count);
        for i in 0..count {
            let start = 2 + i * AllocEvent::SIZE;
            let ev = AllocEvent::from_bytes(&payload[start..start + AllocEvent::SIZE])?;
            events.push(ev);
        }
        Some(Frame::Events(events))
    }

    fn decode_symbols(payload: &[u8]) -> Option<Frame> {
        // payload: [u16 count]([u64 addr][u16 name_len][name UTF-8] × count)
        if payload.len() < 2 {
            return None;
        }
        let count  = u16::from_le_bytes(payload[0..2].try_into().unwrap()) as usize;
        let mut cursor = 2usize;
        let mut syms   = Vec::with_capacity(count);
        for _ in 0..count {
            if cursor + 10 > payload.len() {
                return None;
            }
            let addr     = u64::from_le_bytes(payload[cursor..cursor + 8].try_into().unwrap());
            let name_len = u16::from_le_bytes(payload[cursor + 8..cursor + 10].try_into().unwrap()) as usize;
            cursor += 10;
            if cursor + name_len > payload.len() {
                return None;
            }
            let name = std::str::from_utf8(&payload[cursor..cursor + name_len]).ok()?.to_owned();
            cursor += name_len;
            syms.push((addr, name));
        }
        Some(Frame::Symbols(syms))
    }
}

impl Default for FrameDecoder {
    fn default() -> Self { Self::new() }
}
