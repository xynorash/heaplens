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
                    if self.buf.len() < 4 {
                        return None; // incomplete — not an error
                    }
                    let length = u32::from_le_bytes(self.buf[0..4].try_into().unwrap());
                    if length == 0 || length > MAX_FRAME_LEN {
                        // Untrustworthy length prefix — resync one byte at a time
                        self.buf.drain(0..1);
                        continue;
                    }
                    self.state = DecoderState::NeedBody { total: 4 + length as usize };
                }
                DecoderState::NeedBody { total } => {
                    if self.buf.len() < total {
                        return None; // incomplete — not an error
                    }
                    let frame_bytes: Vec<u8> = self.buf.drain(0..total).collect();
                    self.state = DecoderState::NeedLength;

                    if frame_bytes.len() < 5 {
                        continue;
                    }
                    let ftype = frame_bytes[4];
                    let payload = &frame_bytes[5..];

                    match Self::decode_payload(ftype, payload) {
                        Some(frame) => return Some(frame),
                        None => continue,
                    }
                }
            }
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
