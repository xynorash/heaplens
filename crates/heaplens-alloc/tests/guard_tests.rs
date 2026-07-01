use heaplens_alloc::guard::{force_enter_permanent, is_set, ScopedGuard};

#[test]
fn guard_basics_integration() {
    std::thread::spawn(|| {
        assert!(!is_set());
        let g = ScopedGuard::enter();
        assert!(is_set());
        drop(g);
        assert!(!is_set());
    })
    .join()
    .unwrap();
}

#[test]
fn guard_panic_safe_integration() {
    std::thread::spawn(|| {
        let _ = std::panic::catch_unwind(|| {
            let _g = ScopedGuard::enter();
            panic!("intentional");
        });
        assert!(!is_set());
    })
    .join()
    .unwrap();
}

#[test]
fn force_enter_permanent_integration() {
    std::thread::spawn(|| {
        assert!(!is_set());
        force_enter_permanent();
        assert!(is_set());
    })
    .join()
    .unwrap();
}
