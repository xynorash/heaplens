use heaplens_alloc::ring;
use heaplens_protocol::{AllocEvent, EventKind};

fn ev(n: u64) -> AllocEvent {
    AllocEvent::new(EventKind::Alloc, n, 0, 64, 8, n, [0u64; 16], 0)
}

#[test]
fn thread_ring_drainable_after_exit() {
    let handle = std::thread::spawn(|| {
        ring::push(ev(0xBEEF_CAFE));
    });
    handle.join().unwrap();

    std::thread::sleep(std::time::Duration::from_millis(20));

    // drain_all requires the recursion guard to be permanently set on the
    // calling thread (writer-thread invariant §12.4).
    heaplens_alloc::guard::force_enter_permanent();

    let mut found = false;
    ring::drain_all(|e| {
        if e.ptr == 0xBEEF_CAFE {
            found = true;
        }
    });
    assert!(found, "event from exited thread must remain drainable");
}
