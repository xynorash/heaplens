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
    buf.extend_from_slice(&(payload_len as u32).to_le_bytes());
    buf.push(0x00);
    buf.extend_from_slice(&pid.to_le_bytes());
    buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(name_bytes);
    buf
}

/// Encode an EVENTS frame.
/// Wire: [u32 length][0x01][u16 count][AllocEvent × count]
/// length = 1 + 2 + count * 104
pub fn encode_events(events: &[AllocEvent]) -> Vec<u8> {
    let payload_len = 1 + 2 + events.len() * AllocEvent::SIZE;
    let mut buf = Vec::with_capacity(4 + payload_len);
    buf.extend_from_slice(&(payload_len as u32).to_le_bytes());
    buf.push(0x01);
    buf.extend_from_slice(&(events.len() as u16).to_le_bytes());
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
    buf.extend_from_slice(&(payload_len as u32).to_le_bytes());
    buf.push(0x02);
    buf.extend_from_slice(&(symbols.len() as u16).to_le_bytes());
    for (addr, name) in symbols {
        let name_bytes = name.as_bytes();
        buf.extend_from_slice(&addr.to_le_bytes());
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(name_bytes);
    }
    buf
}
