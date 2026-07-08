use heaplens_protocol::{AllocEvent, EventKind, Frame, FrameDecoder, encode_events};

#[test]
fn single_frame_fed_one_byte_at_a_time() {
    let mut stack = [0u64; 16];
    stack[0] = 0x7fff_cafe_babe_0001;
    let event = AllocEvent::new(
        EventKind::Realloc,
        0x0000_3000_0000_0010,
        0x0000_2fff_ffff_fff0,
        256,
        16,
        5_000_000_000,
        stack,
        1,
    );
    let encoded = encode_events(&[event]);

    let mut dec = FrameDecoder::new();
    let mut frames_seen = 0usize;
    let mut result: Option<Frame> = None;

    for byte in &encoded {
        dec.push(std::slice::from_ref(byte));
        while let Some(frame) = dec.next() {
            frames_seen += 1;
            result = Some(frame);
        }
    }

    assert_eq!(frames_seen, 1, "expected exactly one frame");
    match result.expect("frame should have decoded") {
        Frame::Events(events) => {
            assert_eq!(events.len(), 1);
            assert_eq!(events[0], event);
        }
        other => panic!("wrong variant: {other:?}"),
    }
}
