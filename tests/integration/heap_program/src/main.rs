//! EL0 fixture program for the `tairix-rt` `mem_map`-backed global allocator
//! (the P6e-3b prerequisite increment — `plans/PI.md`).
//!
//! The consuming vertical (`tests/integration/heap_qemu_aarch64`) builds this
//! program into a hardware-isolated EL0 address space, installs a `MemMap`
//! producer backed by `tairix_kernel_mem::map_anonymous` / `unmap_anonymous`
//! over that *live* space, drives the program under the live scheduler, and
//! routes the program's `mem_map` / `mem_unmap` `svc`s — issued by the heap
//! allocator inside `tairix-rt` — through it. The program never calls
//! `mem_map`/`mem_unmap` directly: it uses ordinary `alloc` types and the
//! global allocator turns them into the syscalls.
//!
//! It proves the allocator end to end:
//!
//! 1. A `Box` round-trips a value (a small allocation off the first page).
//! 2. A `Vec` grows across several pages (forcing the arena to grow through
//!    repeated `mem_map`), and every element reads back the value written.
//! 3. After the `Vec` is dropped, a fresh, larger allocation succeeds and
//!    reads back its fill, proving reclaimed space is reusable. The drop
//!    itself does *not* have to reach `mem_unmap`: the arena is resized in
//!    granules, so a few pages coming free stay mapped for the next
//!    allocation.
//! 4. A `Vec` is reserved (forcing the allocator's `realloc` to **grow** the
//!    block) and then `shrink_to_fit` (forcing `realloc` to **shrink** it),
//!    and every original element still reads back — proving `realloc`
//!    preserves the live bytes across both an in-place resize and a move.
//! 5. Reporting a memory-pressure band change drives the heap's trim, which
//!    *is* what returns the arena to the kernel through `mem_unmap`, and a
//!    further allocation then reads back — proving the pages really left the
//!    address space and the arena regrows over them. This is the step that
//!    keeps `mem_unmap` off the dead-code list (the consuming vertical fails
//!    the run if the syscall never arrives).
//!
//! Each step that can fail returns a distinct non-zero exit code; a clean
//! `exit(0)` is the success signal the vertical reports as PASS (fail loud, never silently pass).
//!
//! It is a **pure-Rust** program: it links the Rust userland
//! runtime `tairix-rt` (which supplies both `_start` and the global allocator),
//! never the C ABI. It is built position-independent and
//! converted to an `rxe` blob by the consuming test's build script.
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the crate.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

#[cfg(freestanding)]
extern crate alloc;

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use alloc::boxed::Box;
    use alloc::vec::Vec;

    use tairix_reclaim::PressureBand;

    /// Clean run: every allocation, write, read-back, and free succeeded.
    const EXIT_OK: i32 = 0;
    /// A `Box` did not read back the value stored in it.
    const FAIL_BOX: i32 = 11;
    /// A `Vec` element did not read back the value pushed (the grown pages are
    /// not real read/write memory).
    const FAIL_VEC: i32 = 12;
    /// The post-free reallocation failed or did not read back its fill (freed
    /// arena space is not reusable).
    const FAIL_REUSE: i32 = 13;
    /// A `realloc` (grow via `reserve`, then shrink via `shrink_to_fit`) did
    /// not preserve the vector's contents.
    const FAIL_REALLOC: i32 = 14;
    /// The pressure-driven trim did not report a band change, or the arena
    /// could not be reallocated over after it was handed back.
    const FAIL_TRIM: i32 = 15;

    /// Number of `u32`s the growing `Vec` accumulates: 4096 elements is 16 KiB,
    /// several pages, so the arena must grow through repeated `mem_map`.
    const VEC_LEN: u32 = 4096;

    /// Bytes the post-free reallocation requests (two pages), larger than any
    /// single live allocation before it so it exercises reuse of reclaimed
    /// arena space.
    const REUSE_LEN: usize = 8192;

    /// The value stored at `u32` index `i` of the growing vector: a simple
    /// position-keyed pattern so a stuck or aliased element is caught.
    const fn vec_pattern(i: u32) -> u32 {
        i ^ 0x5555_5555
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime is
    /// set up and routes its return value through the `exit` syscall.
    fn main() -> i32 {
        // 1. Box round-trip — the first allocation maps the first arena page.
        let boxed = Box::new(0xA5A5_5A5Au32);
        if *boxed != 0xA5A5_5A5A {
            return FAIL_BOX;
        }
        drop(boxed);

        // 2. Grow a Vec across several pages and verify every element.
        let mut values: Vec<u32> = Vec::new();
        let mut i = 0u32;
        while i < VEC_LEN {
            values.push(vec_pattern(i));
            i += 1;
        }
        let mut i = 0u32;
        while i < VEC_LEN {
            if values[i as usize] != vec_pattern(i) {
                return FAIL_VEC;
            }
            i += 1;
        }
        drop(values);

        // 3. Reallocate after the free; reclaimed arena space must be reusable.
        let mut reused: Vec<u8> = Vec::new();
        reused.resize(REUSE_LEN, 0xCD);
        if reused.iter().any(|&b| b != 0xCD) {
            return FAIL_REUSE;
        }
        drop(reused);

        // 4. `realloc` must preserve contents across a grow and a shrink.
        // `with_capacity(exact)` then `push` to that capacity leaves the block
        // full, so `reserve` cannot grow in place (the next bytes are the
        // arena top span or beyond) and exercises the grow path; the trailing
        // `shrink_to_fit` exercises the shrink path. Either way the original
        // elements must survive.
        let mut grown: Vec<u32> = Vec::with_capacity(8);
        let mut i = 0u32;
        while i < 8 {
            grown.push(vec_pattern(i));
            i += 1;
        }
        grown.reserve(VEC_LEN as usize);
        let mut i = 0u32;
        while i < 8 {
            grown.push(vec_pattern(i + 8));
            i += 1;
        }
        grown.shrink_to_fit();
        let mut i = 0u32;
        while i < 16 {
            if grown[i as usize] != vec_pattern(i) {
                return FAIL_REALLOC;
            }
            i += 1;
        }
        drop(grown);

        // 5. A band change drives the heap's exact trim, which hands the free
        // arena top back through `mem_unmap`. Critical is the band an
        // unreported gauge already answers, so a comfortable band has to be
        // reported first for the second report to be a change at all.
        if !tairix_rt::pressure::report(PressureBand::Normal) {
            return FAIL_TRIM;
        }
        if !tairix_rt::pressure::report(PressureBand::Critical) {
            return FAIL_TRIM;
        }
        // The arena regrows over the range just unmapped, so a wrong unmap —
        // pages left mapped but untracked, or a live range released — shows up
        // here as a fault or a bad read-back rather than passing silently.
        let mut regrown: Vec<u8> = Vec::new();
        regrown.resize(REUSE_LEN, 0x5C);
        if regrown.iter().any(|&b| b != 0x5C) {
            return FAIL_TRIM;
        }
        drop(regrown);

        EXIT_OK
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the freestanding
// `tairix-rt` entry path is not compiled, so this inert `main` keeps the crate
// building under the host tooling. It performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
