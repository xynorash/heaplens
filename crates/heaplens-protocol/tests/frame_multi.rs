use heaplens_protocol::{AllocEvent, EventKind, Frame, FrameDecoder,
                        encode_events, encode_handshake, encode_symbols};

#[test]
fn three_frames_in_one_push() {
    let event = AllocEvent::new(
        EventKind::Alloc, 0x1000_0000_0001, 0, 128, 8, 42_000_000, [0u64; 8], 0,
    );
    let f1 = encode_handshake(99, "multi-test");
    let f2 = encode_events(&[event]);
    let f3 = encode_symbols(&[(0x7fff_1234_5678, "some::symbol")]);

    let mut combined = Vec::new();
    combined.extend_from_slice(&f1);
    combined.extend_from_slice(&f2);
    combined.extend_from_slice(&f3);

    let mut dec = FrameDecoder::new();
    dec.push(&combined);

    match dec.next().expect("frame 1") {
        Frame::Handshake { pid, name } => {
            assert_eq!(pid, 99);
            assert_eq!(name, "multi-test");
        }
        other => panic!("frame 1 wrong variant: {other:?}"),
    }

    match dec.next().expect("frame 2") {
        Frame::Events(events) => {
            assert_eq!(events.len(), 1);
            assert_eq!(events[0], event);
        }
        other => panic!("frame 2 wrong variant: {other:?}"),
    }

    match dec.next().expect("frame 3") {
        Frame::Symbols(syms) => {
            assert_eq!(syms.len(), 1);
            assert_eq!(syms[0], (0x7fff_1234_5678, "some::symbol".to_owned()));
        }
        other => panic!("frame 3 wrong variant: {other:?}"),
    }

    assert!(dec.next().is_none());
}
