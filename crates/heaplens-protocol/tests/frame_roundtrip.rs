use heaplens_protocol::{
    AllocEvent, EventKind, Frame, FrameDecoder,
    encode_events, encode_handshake, encode_symbols,
};

fn make_decoder_with(bytes: &[u8]) -> FrameDecoder {
    let mut d = FrameDecoder::new();
    d.push(bytes);
    d
}

#[test]
fn handshake_round_trip() {
    let encoded = encode_handshake(12345, "test-process");
    let mut dec = make_decoder_with(&encoded);
    match dec.next().expect("expected a frame") {
        Frame::Handshake { pid, name } => {
            assert_eq!(pid, 12345);
            assert_eq!(name, "test-process");
        }
        other => panic!("wrong variant: {other:?}"),
    }
    assert!(dec.next().is_none());
}

fn sample_event(n: u64) -> AllocEvent {
    let mut stack = [0u64; 8];
    stack[0] = 0x7fff_0000_0000_0000 + n;
    AllocEvent::new(EventKind::Alloc, 0x2000_0000_0000 + n, 0, 64 + n, 8, 1_000_000 + n, stack, 1)
}

#[test]
fn events_round_trip_multiple() {
    let events = vec![sample_event(1), sample_event(2), sample_event(3)];
    let encoded = encode_events(&events);
    let mut dec = make_decoder_with(&encoded);
    match dec.next().expect("expected a frame") {
        Frame::Events(decoded) => {
            assert_eq!(decoded.len(), 3);
            assert_eq!(decoded[0], events[0]);
            assert_eq!(decoded[1], events[1]);
            assert_eq!(decoded[2], events[2]);
        }
        other => panic!("wrong variant: {other:?}"),
    }
    assert!(dec.next().is_none());
}

#[test]
fn symbols_round_trip_multiple() {
    let syms: Vec<(u64, &str)> = vec![
        (0x7fff_dead_0001, "alloc::vec::Vec::push"),
        (0x7fff_dead_0002, "std::collections::HashMap::insert"),
        (0x7fff_dead_0003, "my_crate::foo::bar"),
    ];
    let encoded = encode_symbols(&syms);
    let mut dec = make_decoder_with(&encoded);
    match dec.next().expect("expected a frame") {
        Frame::Symbols(decoded) => {
            assert_eq!(decoded.len(), 3);
            assert_eq!(decoded[0], (syms[0].0, syms[0].1.to_owned()));
            assert_eq!(decoded[1], (syms[1].0, syms[1].1.to_owned()));
            assert_eq!(decoded[2], (syms[2].0, syms[2].1.to_owned()));
        }
        other => panic!("wrong variant: {other:?}"),
    }
    assert!(dec.next().is_none());
}
