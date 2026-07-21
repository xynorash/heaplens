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
    let mut stack = [0u64; 16];
    stack[0] = 0x7fff_0000_0000_0000 + n;
    AllocEvent::new(EventKind::Alloc, 0x2000_0000_0000 + n, 0, 64 + n, 8, 1_000_000 + n, stack, 1)
}

#[test]
fn events_round_trip_multiple() {
    let events = vec![sample_event(1), sample_event(2), sample_event(3)];
    let encoded = encode_events(&events).expect("well under u16::MAX");
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

/// Regression for the writer-thread panic found under sustained 12-thread
/// injection load (2026-07-22): `encode_events`/`encode_symbols` used to
/// `.expect()` the `u16` count conversion, panicking the writer thread on an
/// oversized batch and breaking clean detach (a panicked writer never calls
/// `mark_writer_stopped`). The real fix is capping `ring::drain_all` so a
/// batch can never structurally reach this size — this test covers the
/// defense-in-depth boundary itself: given a batch that does exceed
/// `u16::MAX` (however that came to be), encoding must fail gracefully
/// (`None`), never panic.
#[test]
fn encode_events_returns_none_instead_of_panicking_when_over_u16_max() {
    let event = sample_event(1);
    let oversized: Vec<AllocEvent> = std::iter::repeat(event)
        .take(u16::MAX as usize + 1)
        .collect();
    assert_eq!(encode_events(&oversized), None, "must fail gracefully, not panic");

    // One under the limit still encodes fine — confirms the boundary is
    // exactly u16::MAX, not off-by-one in either direction.
    let at_limit: Vec<AllocEvent> = std::iter::repeat(event)
        .take(u16::MAX as usize)
        .collect();
    assert!(encode_events(&at_limit).is_some(), "exactly u16::MAX must still encode");
}

#[test]
fn encode_symbols_returns_none_instead_of_panicking_when_over_u16_max() {
    let oversized: Vec<(u64, &str, bool)> = std::iter::repeat((0x1234u64, "sym", false))
        .take(u16::MAX as usize + 1)
        .collect();
    assert_eq!(encode_symbols(&oversized), None, "must fail gracefully, not panic");
}

#[test]
fn symbols_round_trip_multiple() {
    let syms: Vec<(u64, &str, bool)> = vec![
        (0x7fff_dead_0001, "alloc::vec::Vec::push", true),
        (0x7fff_dead_0002, "std::collections::HashMap::insert", false),
        (0x7fff_dead_0003, "my_crate::foo::bar", false),
    ];
    let encoded = encode_symbols(&syms).expect("well under u16::MAX");
    let mut dec = make_decoder_with(&encoded);
    match dec.next().expect("expected a frame") {
        Frame::Symbols(decoded) => {
            assert_eq!(decoded.len(), 3);
            assert_eq!(decoded[0], (syms[0].0, syms[0].1.to_owned(), syms[0].2));
            assert_eq!(decoded[1], (syms[1].0, syms[1].1.to_owned(), syms[1].2));
            assert_eq!(decoded[2], (syms[2].0, syms[2].1.to_owned(), syms[2].2));
        }
        other => panic!("wrong variant: {other:?}"),
    }
    assert!(dec.next().is_none());
}
