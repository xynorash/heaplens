use heaplens_protocol::{AllocEvent, EventKind, Frame, FrameDecoder,
                        encode_events, encode_handshake};

#[test]
fn resync_after_absurd_length_prefix() {
    // Two valid frames sandwiching junk with an absurd length prefix.
    // The junk bytes must not prevent either valid frame from decoding.
    let event = AllocEvent::new(
        EventKind::Dealloc, 0x5555_0000_0001, 0, 64, 8, 1, [0u64; 16], 0,
    );
    let valid1 = encode_handshake(1, "before-junk");
    let valid2 = encode_events(&[event]).expect("well under u16::MAX");

    // Junk: a u32 length prefix of 0xFF_FF_FF_FF (> MAX_FRAME_LEN = 8 MiB),
    // followed by a few bytes. The decoder must drain one byte at a time until
    // it resyncs onto valid2's length prefix.
    let junk: Vec<u8> = {
        let mut j = Vec::new();
        j.extend_from_slice(&u32::MAX.to_le_bytes()); // absurd length
        j.extend_from_slice(&[0xAA, 0xBB, 0xCC]);    // padding noise
        j
    };

    let mut combined = Vec::new();
    combined.extend_from_slice(&valid1);
    combined.extend_from_slice(&junk);
    combined.extend_from_slice(&valid2);

    let mut dec = FrameDecoder::new();
    dec.push(&combined);

    // First valid frame decodes before the junk
    match dec.next().expect("frame 1 (before junk)") {
        Frame::Handshake { pid, name } => {
            assert_eq!(pid, 1);
            assert_eq!(name, "before-junk");
        }
        other => panic!("frame 1 wrong variant: {other:?}"),
    }

    // Second valid frame decodes after the decoder resyncs past the junk
    match dec.next().expect("frame 2 (after junk)") {
        Frame::Events(events) => {
            assert_eq!(events.len(), 1);
            assert_eq!(events[0], event);
        }
        other => panic!("frame 2 wrong variant: {other:?}"),
    }

    assert!(dec.next().is_none());
}
