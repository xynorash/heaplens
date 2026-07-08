#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EventKind {
    Alloc   = 0,
    Dealloc = 1,
    Realloc = 2,
}

impl EventKind {
    pub fn from_u8(v: u8) -> Option<EventKind> {
        match v {
            0 => Some(EventKind::Alloc),
            1 => Some(EventKind::Dealloc),
            2 => Some(EventKind::Realloc),
            _ => None,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AllocEvent {
    pub kind:      u8,
    pub stack_len: u8,
    pub _pad:      [u8; 2],
    pub align:     u32,
    pub ptr:       u64,
    pub old_ptr:   u64,
    pub size:      u64,
    pub ts_nanos:  u64,
    /// Raw, unfiltered instruction pointers from the capture point downward.
    /// 16 frames (up from 8): the zeroed-allocation path (`vec![0u8; n]`,
    /// the idiom used throughout this project's producers) inserts 6+
    /// non-inlined std frames between the shared instrumentation and the
    /// real call site, so 8 frames can contain zero user frames. Consumers
    /// (daemon-side phi matching, symbol display) locate the real call site
    /// by classifying frames via the writer-resolved SYMBOLS frame, not by
    /// a fixed index — see heaplens-daemon's graph.rs `effective_site`.
    pub stack:     [u64; 16],
}

const _: () = assert!(core::mem::size_of::<AllocEvent>() == AllocEvent::SIZE);

impl AllocEvent {
    pub const SIZE: usize = 168;

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind:      EventKind,
        ptr:       u64,
        old_ptr:   u64,
        size:      u64,
        align:     u32,
        ts_nanos:  u64,
        stack:     [u64; 16],
        stack_len: u8,
    ) -> Self {
        AllocEvent {
            kind: kind as u8,
            stack_len,
            _pad: [0, 0],
            align,
            ptr,
            old_ptr,
            size,
            ts_nanos,
            stack,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: AllocEvent is repr(C) with an explicit `_pad` field, so
        // there is no implicit compiler padding — all SIZE bytes belong to
        // initialized fields. Lifetime is tied to &self.
        // (AllocEvent::new zero-initializes `_pad` for deterministic, leak-free
        // wire output — a contract concern, not a soundness one.)
        unsafe {
            std::slice::from_raw_parts(self as *const AllocEvent as *const u8, Self::SIZE)
        }
    }

    pub fn from_bytes(buf: &[u8]) -> Option<AllocEvent> {
        if buf.len() < Self::SIZE {
            return None;
        }
        // SAFETY: length checked above; read_unaligned makes no alignment
        // assumption about the incoming byte buffer.
        Some(unsafe { (buf.as_ptr() as *const AllocEvent).read_unaligned() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t1_size_of_alloc_event() {
        assert_eq!(core::mem::size_of::<AllocEvent>(), 168);
        assert_eq!(AllocEvent::SIZE, 168);
    }

    #[test]
    fn t2_round_trip() {
        let mut stack = [0u64; 16];
        stack[0] = 0x0000_7fff_dead_beef;
        stack[1] = 0x0000_7fff_cafe_babe;
        stack[2] = 0x0000_7fff_1234_5678;

        let original = AllocEvent::new(
            EventKind::Realloc,
            0x0000_2000_0000_0010,
            0x0000_1fff_ffff_fff0,
            0x0000_0000_0001_0000,
            16,
            9_999_999_999,
            stack,
            3,
        );

        let bytes = original.as_bytes();
        assert_eq!(bytes.len(), AllocEvent::SIZE);

        let decoded = AllocEvent::from_bytes(bytes).expect("from_bytes failed");
        assert_eq!(decoded, original);
    }

    #[test]
    fn t2_from_bytes_too_short() {
        let short = [0u8; 10];
        assert!(AllocEvent::from_bytes(&short).is_none());
    }

    #[test]
    fn eventkind_from_u8() {
        assert_eq!(EventKind::from_u8(0), Some(EventKind::Alloc));
        assert_eq!(EventKind::from_u8(1), Some(EventKind::Dealloc));
        assert_eq!(EventKind::from_u8(2), Some(EventKind::Realloc));
        assert_eq!(EventKind::from_u8(3), None);
        assert_eq!(EventKind::from_u8(255), None);
    }
}
