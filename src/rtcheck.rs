//! A real-time safety check: catches memory allocation on the audio thread.
//!
//! Since step 2 the whole design has rested on one rule: the audio
//! callback never allocates or frees memory, because the allocator can
//! take a lock or ask the OS for memory, and either can stall for longer
//! than a buffer lasts. Until now nothing CHECKED that rule automatically.
//!
//! Rust lets a program replace its global allocator: the code behind every
//! `Box`, `Vec` and `String`. `CheckingAllocator` passes everything to the
//! system allocator, but first looks at a thread-local flag that the engine
//! raises for the whole of each audio callback (`Zone`). Any allocation,
//! reallocation or free while the flag is up is counted.
//!
//! It's installed in tests and debug builds (see main.rs). Release builds
//! use the plain system allocator; only the flag write remains, which costs
//! next to nothing. (The `assert_no_alloc` crate does the same job.)

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    // `const`-initialized with no destructor: safe to touch from inside the
    // allocator itself, which must never allocate to do its own job.
    static IN_ZONE: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

fn note() {
    // `try_with`: during thread shutdown thread-locals may be gone.
    let _ = IN_ZONE.try_with(|z| {
        if z.get() {
            let _ = COUNT.try_with(|c| c.set(c.get() + 1));
        }
    });
}

pub struct CheckingAllocator;

// SAFETY: every call is forwarded unchanged to the system allocator.
unsafe impl GlobalAlloc for CheckingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note();
        System.dealloc(ptr, layout)
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        System.alloc_zeroed(layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        System.realloc(ptr, layout, new_size)
    }
}

/// While alive, allocations on this thread are counted as violations.
pub struct Zone {
    outer: bool,
    start: u64,
}

impl Zone {
    pub fn enter() -> Self {
        let outer = IN_ZONE.with(|z| z.replace(true));
        Self { outer, start: COUNT.with(|c| c.get()) }
    }

    /// Violations inside this zone so far.
    pub fn violations(&self) -> u64 {
        COUNT.with(|c| c.get()) - self.start
    }
}

impl Drop for Zone {
    fn drop(&mut self) {
        IN_ZONE.with(|z| z.set(self.outer));
    }
}

/// Runs `f` in a zone; returns its result and how many times it allocated.
#[cfg(test)]
pub fn count_allocations<R>(f: impl FnOnce() -> R) -> (R, u64) {
    let zone = Zone::enter();
    let r = f();
    let n = zone.violations();
    drop(zone);
    (r, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_in_a_zone_is_caught() {
        let (_, n) = count_allocations(|| drop(std::hint::black_box(vec![1u8; 100])));
        assert!(n >= 2, "an allocation and a free: {n}");
        let mut v: Vec<u8> = Vec::with_capacity(16);
        let (_, n) = count_allocations(|| {
            for i in 0..16 {
                v.push(i); // within capacity: no allocation
            }
        });
        assert_eq!(n, 0);
        let (_, n) = count_allocations(|| v.push(99)); // over capacity: grows
        assert_eq!(n, 1);
    }

    #[test]
    fn outside_a_zone_nothing_is_counted() {
        let zone = Zone::enter();
        drop(zone);
        let before = COUNT.with(|c| c.get());
        let _ = std::hint::black_box(String::from("free to allocate here"));
        assert_eq!(COUNT.with(|c| c.get()), before);
    }
}
