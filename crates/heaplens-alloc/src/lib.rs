mod guard;
mod ring;
mod capture;
mod writer;

use std::alloc::{GlobalAlloc, Layout, System};
use heaplens_protocol::EventKind;

/// Global allocator that intercepts all allocations and ships events
/// off-process over a named pipe without blocking.
///
/// Usage:
/// ```no_run
/// use heaplens_alloc::HeapLensAlloc;
/// #[global_allocator]
/// static GLOBAL: HeapLensAlloc = HeapLensAlloc::new();
/// ```
pub struct HeapLensAlloc;

impl HeapLensAlloc {
    pub const fn new() -> Self { HeapLensAlloc }
}

unsafe impl GlobalAlloc for HeapLensAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        System.realloc(ptr, layout, new_size)
    }
}
