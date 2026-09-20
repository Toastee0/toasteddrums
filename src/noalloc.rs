//! noalloc — a debug-build guard that catches allocation on the audio thread.
//!
//! WHY: allocating inside the audio callback takes a global lock and sometimes a syscall, on
//! a thread with a ~10 ms deadline. It never fails loudly. It fails as an intermittent click
//! that shows up under load, on someone else's machine, months later — exactly the bug the
//! waveOut work already cost us once. This turns it into a counted fault at the moment it
//! happens, so a regression is caught by `cargo test` rather than by ear.
//!
//! Three things about the implementation are load-bearing:
//!
//! - The flag is a `const`-initialised thread_local. A lazily initialised one allocates on
//!   first touch, and allocating from inside the allocator recurses forever.
//! - `try_with`, because the flag is read during thread teardown too, after the TLS is gone.
//! - Nothing panics or prints from inside `alloc`. The panic machinery allocates, and so
//!   does formatting. A violation only bumps a counter; whoever set the flag reads it after.
//!
//! In release the whole check compiles to nothing and the allocator is `System` with no
//! branch in front of it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct Guard;

static VIOLATIONS: AtomicUsize = AtomicUsize::new(0);

#[cfg(debug_assertions)]
thread_local! {
    static FORBIDDEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(debug_assertions)]
#[inline]
fn note() {
    let _ = FORBIDDEN.try_with(|f| {
        if f.get() { VIOLATIONS.fetch_add(1, Ordering::Relaxed); }
    });
}

#[cfg(not(debug_assertions))]
#[inline(always)]
fn note() {}

unsafe impl GlobalAlloc for Guard {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 { note(); System.alloc(l) }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 { note(); System.alloc_zeroed(l) }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 { note(); System.realloc(p, l, n) }
    // Freeing takes the same lock as allocating, so it is a fault on the audio thread too.
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) { note(); System.dealloc(p, l) }
}

/// Runs `f` with allocation forbidden on this thread. Nested calls are safe: the previous
/// setting is restored, so a guarded region inside a guarded region still ends correctly.
///
/// This does not prevent allocation, it records it. The callback still runs; `violations`
/// reports afterwards.
pub fn forbidden<R>(f: impl FnOnce() -> R) -> R {
    #[cfg(debug_assertions)]
    {
        let prev = FORBIDDEN.try_with(|c| c.replace(true)).unwrap_or(false);
        let r = f();
        let _ = FORBIDDEN.try_with(|c| c.set(prev));
        r
    }
    #[cfg(not(debug_assertions))]
    f()
}

/// Total allocations seen inside a `forbidden` region since the process started, across all
/// threads. Always 0 in release.
pub fn violations() -> usize { VIOLATIONS.load(Ordering::Relaxed) }

#[cfg(test)]
mod tests {
    #[test]
    fn the_guard_sees_an_allocation_and_ignores_one_outside() {
        let before = super::violations();
        super::forbidden(|| { let v: Vec<u8> = Vec::with_capacity(64); std::hint::black_box(&v); });
        let caught = super::violations();
        assert!(caught > before, "an allocation inside a forbidden region must be counted");

        let v: Vec<u8> = Vec::with_capacity(64);
        std::hint::black_box(&v);
        drop(v);
        // Allocating outside a region is ordinary and must not move the counter. (The drop
        // of the Vec above happens outside too, so a dealloc must not count either.)
        assert_eq!(super::violations(), caught, "allocation outside a region must be ignored");
    }
}
