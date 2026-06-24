//! SPAWN stage `SP5b-2` fixture program: a minimal, separately-linked
//! pure-Rust EL0 program that exercises the `abi-v1` anonymous memory
//! map/unmap pair (`plans/SPAWN.md` SP5).
//!
//! The consuming vertical (`tests/integration/mem_map_qemu_aarch64`) builds
//! this program into a hardware-isolated EL0 address space, installs a
//! `MemMap` producer backed by `rustos_kernel_mem::map_anonymous` /
//! `unmap_anonymous` over that *live* space, and drives the program under the
//! live scheduler. The program:
//!
//! 1. `mem_map`s a fresh anonymous region at a FIXED virtual address (so the
//!    kernel's fault handler knows exactly where the post-unmap fault must
//!    land), proving the kernel returns the requested base.
//! 2. Writes a deterministic pattern across the whole region and reads it
//!    back, proving the pages are genuine, zeroed-then-writable `RW` memory.
//! 3. `mem_unmap`s the region.
//! 4. Touches the now-released range, which must raise a data abort — the
//!    fault-on-use-after-unmap the vertical reports as PASS.
//!
//! Every step that can fail returns a distinct non-zero exit code instead of
//! reaching the deliberate fault, so the kernel side reports a failure rather
//! than the program silently passing. Reaching the
//! final `return` at all (no fault) is itself a failure code.
//!
//! It is a **pure-Rust** program: it links the Rust userland
//! runtime `rustos-rt` and the shared `abi-v1` types (`rustos-abi`), never the
//! C ABI (`crt0` + `abi-sys`), which exists solely for non-Rust programs. It is built position-independent and converted to an
//! `rxe` blob by the consuming test's build script. On
//! the host it is an inert stub so `cargo build --workspace`, clippy, and fmt
//! still cover the crate.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use rustos_abi::MapFlags;

    /// Default region base when the consuming build did not pin one through
    /// `RUSTOS_MEM_MAP_ADDR`. 64 GiB + 16 MiB — well above both the kernel's
    /// 2 GiB identity window and the program image / stack / startup block at
    /// the 64 GiB bias — so the region lands on freshly walked stage-1 tables.
    const DEFAULT_REGION_VA: u64 = (64 << 30) + (16 << 20);

    /// Default region length (two pages) when the consuming build did not pin
    /// one through `RUSTOS_MEM_MAP_LEN`.
    const DEFAULT_REGION_LEN: usize = 2 * 4096;

    /// Virtual base the region is mapped at. The consuming vertical's build
    /// script sets `RUSTOS_MEM_MAP_ADDR` (decimal) so the program and the
    /// kernel's fault check agree on the address.
    const REGION_VA: u64 = match option_env!("RUSTOS_MEM_MAP_ADDR") {
        Some(s) => parse_u64(s.as_bytes(), DEFAULT_REGION_VA),
        None => DEFAULT_REGION_VA,
    };

    /// Length in bytes of the region, pinned by `RUSTOS_MEM_MAP_LEN` (decimal).
    const REGION_LEN: usize = match option_env!("RUSTOS_MEM_MAP_LEN") {
        Some(s) => parse_u64(s.as_bytes(), DEFAULT_REGION_LEN as u64) as usize,
        None => DEFAULT_REGION_LEN,
    };

    /// Exit code: `mem_map` failed or did not honour the FIXED placement.
    const FAIL_MAP: i32 = 11;
    /// Exit code: the region did not read back the pattern that was written
    /// (the mapped pages are not real read/write memory).
    const FAIL_VERIFY: i32 = 12;
    /// Exit code: `mem_unmap` returned an error.
    const FAIL_UNMAP: i32 = 13;
    /// Exit code: touching the released range did *not* fault (use-after-unmap
    /// was wrongly permitted — an isolation/teardown regression).
    const FAIL_NO_FAULT: i32 = 14;

    /// Parse `bytes` as a non-negative decimal integer at compile time,
    /// falling back to `default` on an empty string, a non-digit byte, or
    /// overflow of the `u64` range. `const` and panic-free so the values are
    /// fixed into the image with no runtime parsing (fail
    /// closed to the default).
    const fn parse_u64(bytes: &[u8], default: u64) -> u64 {
        let mut acc: u64 = 0;
        let mut i = 0usize;
        let mut seen = false;
        while i < bytes.len() {
            let b = bytes[i];
            if b < b'0' || b > b'9' {
                return default;
            }
            let digit = (b - b'0') as u64;
            acc = match acc.checked_mul(10) {
                Some(v) => match v.checked_add(digit) {
                    Some(v) => v,
                    None => return default,
                },
                None => return default,
            };
            seen = true;
            i += 1;
        }
        if seen {
            acc
        } else {
            default
        }
    }

    /// The byte written at offset `i` of the region: a simple position-keyed
    /// pattern so a stuck or aliased byte is caught by the read-back.
    const fn pattern_byte(i: usize) -> u8 {
        ((i as u8) ^ 0xA5).wrapping_add(0x11)
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall — so
    /// any `return` here is observed by the kernel as an `exit` (a failure,
    /// since the success path is the deliberate fault that never returns).
    fn main() -> i32 {
        // 1. Map a fresh anonymous RW region at exactly REGION_VA.
        let base = rustos_rt::mem_map(REGION_LEN, MapFlags::FIXED, REGION_VA);
        if base < 0 || base as u64 != REGION_VA {
            return FAIL_MAP;
        }
        let region = REGION_VA as *mut u8;

        // 2a. Write the pattern across the whole region.
        let mut i = 0usize;
        while i < REGION_LEN {
            // SAFETY: `mem_map` just returned REGION_VA as the base of a
            // REGION_LEN-byte RW|USER region in this process's own address
            // space, so `region.add(i)` is a valid, writable, in-bounds
            // pointer for `i < REGION_LEN` (the kernel
            // validated and installed the mapping).
            unsafe {
                region.add(i).write_volatile(pattern_byte(i));
            }
            i += 1;
        }

        // 2b. Read it back and verify — proves the pages are real RW memory.
        let mut i = 0usize;
        while i < REGION_LEN {
            // SAFETY: as above; the region is mapped READ|WRITE|USER and the
            // index is in bounds.
            let got = unsafe { region.add(i).read_volatile() };
            if got != pattern_byte(i) {
                return FAIL_VERIFY;
            }
            i += 1;
        }

        // 3. Release the region.
        if rustos_rt::mem_unmap(REGION_VA, REGION_LEN) != 0 {
            return FAIL_UNMAP;
        }

        // 4. Touch the released range. The mapping is gone, so this must raise
        //    a data abort the kernel's fault handler reports as PASS. The read
        //    is `volatile` so the compiler cannot elide the access that must
        //    fault.
        // SAFETY: this access is *expected* to fault — the range was just
        // unmapped. If the teardown wrongly left it mapped the read is still
        // of a valid pointer-sized location, and the program then returns the
        // FAIL_NO_FAULT code below (fail loud, never hang).
        let observed = unsafe { region.read_volatile() };
        // Reaching here means no fault fired — use-after-unmap was permitted.
        // Reference `observed` so the read is not optimised away.
        let _ = observed;
        FAIL_NO_FAULT
    }

    rustos_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the freestanding
// `rustos-rt` entry path is not compiled, so this inert `main` keeps the crate
// building under the host tooling. It performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
