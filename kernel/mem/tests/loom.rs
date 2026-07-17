//! Loom model-checking harness for the frame allocator.
//!
//! Run with:
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test --test loom \
//!     -p tairix-kernel-mem --release
//! ```
//!
//! When the `loom` cfg is *not* set this file compiles to an empty test
//! binary so the default `cargo test` workflow stays fast (mirrors the
//! convention established by `kernel/sync/tests/loom.rs`).
//!
//! We exercise only the public allocator API. Loom traps every
//! `SpinLock::lock` / `unlock` performed by `FrameAllocator`'s internal
//! lock and explores every legal interleaving.

#![cfg(loom)]

use loom::sync::Arc;
use loom::thread;

use tairix_kernel_mem::{
    BootMemoryMap, FrameAllocator, MemoryRegion, PhysAddr, RegionKind, PAGE_SIZE,
};

fn small_map(pages: usize) -> BootMemoryMap {
    let mut m = BootMemoryMap::new();
    m.push(MemoryRegion {
        start: PhysAddr::new(0),
        length: (pages * PAGE_SIZE) as u64,
        kind: RegionKind::Usable,
    });
    m
}

#[test]
fn concurrent_alloc_does_not_double_hand_out() {
    loom::model(|| {
        let a = Arc::new(FrameAllocator::new(&small_map(4)).unwrap());
        let a1 = a.clone();

        let t1 = thread::spawn(move || a1.alloc().ok());
        let f0 = a.alloc().ok();
        let f1 = t1.join().unwrap();

        match (f0, f1) {
            (Some(x), Some(y)) => assert_ne!(x, y, "two threads got the same frame"),
            (Some(_), None) | (None, Some(_)) | (None, None) => {
                // Either ordering is allowed; only "both got a frame and
                // they collide" is a bug.
            }
        }
    });
}

#[test]
fn alloc_then_free_round_trip_concurrent() {
    loom::model(|| {
        let a = Arc::new(FrameAllocator::new(&small_map(2)).unwrap());
        let a1 = a.clone();
        let t1 = thread::spawn(move || {
            if let Ok(f) = a1.alloc() {
                a1.free(f).unwrap();
            }
        });
        if let Ok(f) = a.alloc() {
            a.free(f).unwrap();
        }
        t1.join().unwrap();
        // After both threads have completed all allocs+frees, every
        // frame must be free again.
        assert_eq!(a.free_frames(), 2);
    });
}
