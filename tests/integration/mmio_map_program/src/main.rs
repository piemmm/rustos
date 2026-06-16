//! PI P10 chunk 5d-0-ii (b′) fixture program: a minimal, separately-linked
//! pure-Rust EL0 program that exercises the `abi-v1` `mmio_map` syscall
//! (`plans/PI.md` 5d-0-ii (b′)-2).
//!
//! The consuming vertical (`tests/integration/mmio_map_qemu_aarch64`) builds
//! this program into a hardware-isolated EL0 address space, **retains that
//! space live** (the production `rustos_kernel_core::spawn_user_kthread_with_stack_live`
//! path), mints the calling task a device-resource grant for a real `virt`
//! virtio-MMIO transport window, and routes the program's `mmio_map` syscall
//! through the production owner-checked resolution + the retained-space
//! `rustos_kernel_mem::LiveSpace::map_device_window` mechanism. The program:
//!
//! 1. `mmio_map`s its granted device window by handle, proving the kernel
//!    resolved the grant for *this* task and mapped a real, caching-disabled
//!    device window into the program's own address space, returning its base
//!    virtual address.
//! 2. Reads the device's first register (the virtio-MMIO `MagicValue` at
//!    offset 0) through that base and checks it equals the expected magic —
//!    proving the window points at genuine device MMIO, not blank memory.
//! 3. Returns `0` (PASS) on a match, or a distinct non-zero code on any
//!    failure, which `rustos-rt` routes through `exit`; the vertical reports
//!    exit code `0` as PASS and any other as a failure (`AGENTS.md` §7 / §2.9).
//!
//! It is a **pure-Rust** program (`AGENTS.md` §1): it links the Rust userland
//! runtime `rustos-rt`, never the C ABI (`crt0` + `abi-sys`), which exists
//! solely for non-Rust programs (`AGENTS.md` §16.4). It is built
//! position-independent and converted to an `rxe` blob by the consuming test's
//! build script (`AGENTS.md` §9, §19.2). On the host it is an inert stub so
//! `cargo build --workspace`, clippy, and fmt still cover the crate.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    /// Default grant handle when the consuming build did not pin one through
    /// `RUSTOS_MMIO_GRANT_HANDLE`. The first device-resource grant a task is
    /// minted is handle `1` (the registry issues per-task handles monotonic
    /// from `1`, with `0` reserved-invalid), and the vertical mints exactly
    /// one grant for this task, so `1` names this program's only window.
    const DEFAULT_GRANT_HANDLE: u64 = 1;

    /// Default expected first-register value when the consuming build did not
    /// pin one through `RUSTOS_MMIO_MAGIC`: the virtio-MMIO `MagicValue`
    /// register ("virt", little-endian), which a QEMU `virt` virtio-MMIO
    /// transport reports at offset 0 unconditionally.
    const DEFAULT_MAGIC: u32 = 0x7472_6976;

    /// Default offset of the register read back, when the consuming build did
    /// not pin one through `RUSTOS_MMIO_REG_OFFSET` (the virtio-MMIO
    /// `MagicValue` is at offset 0).
    const DEFAULT_REG_OFFSET: u64 = 0;

    /// The grant handle this program maps (pinned by the consuming build, the
    /// §2.2 single source of truth, else the default).
    const GRANT_HANDLE: u64 = match option_env!("RUSTOS_MMIO_GRANT_HANDLE") {
        Some(s) => parse_u64(s.as_bytes(), DEFAULT_GRANT_HANDLE),
        None => DEFAULT_GRANT_HANDLE,
    };

    /// The expected first-register value (pinned by the consuming build, else
    /// the default).
    #[allow(clippy::cast_possible_truncation)]
    const MAGIC: u32 = match option_env!("RUSTOS_MMIO_MAGIC") {
        Some(s) => parse_u64(s.as_bytes(), DEFAULT_MAGIC as u64) as u32,
        None => DEFAULT_MAGIC,
    };

    /// The offset of the register read back (pinned by the consuming build,
    /// else the default).
    const REG_OFFSET: u64 = match option_env!("RUSTOS_MMIO_REG_OFFSET") {
        Some(s) => parse_u64(s.as_bytes(), DEFAULT_REG_OFFSET),
        None => DEFAULT_REG_OFFSET,
    };

    /// Exit code: `mmio_map` returned an error (a negative `-errno`) — the
    /// kernel refused to map the granted window.
    const FAIL_MAP: i32 = 11;
    /// Exit code: the mapped device register did not read back the expected
    /// magic (the window does not point at the granted device MMIO).
    const FAIL_MAGIC: i32 = 12;

    /// Parse `bytes` as a non-negative decimal integer at compile time,
    /// falling back to `default` on an empty string, a non-digit byte, or
    /// overflow of the `u64` range. `const` and panic-free so the values are
    /// fixed into the image with no runtime parsing (`AGENTS.md` §2.9 — fail
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

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall — so a
    /// `0` return is observed by the kernel as `exit(0)` (PASS) and any other
    /// as a failure code.
    fn main() -> i32 {
        // 1. Map the granted device window by handle. A negative result is the
        //    `-errno` the kernel returned (a refused or unresolved grant).
        let base = rustos_rt::mmio_map(GRANT_HANDLE);
        if base < 0 {
            return FAIL_MAP;
        }
        #[allow(clippy::cast_sign_loss)] // `base >= 0` checked above; it is a user VA.
        let reg = (base as u64 + REG_OFFSET) as *const u32;

        // 2. Read the device's first register through the mapped window. The
        //    read is `volatile` so the compiler cannot elide the access to the
        //    caching-disabled device register.
        // SAFETY: `mmio_map` returned the base of a mapped, caching-disabled,
        //    USER-readable device window of at least `REG_OFFSET + 4` bytes in
        //    this process's own address space, so `reg` is a valid, readable,
        //    in-bounds pointer (`AGENTS.md` §5.4 — the kernel validated the
        //    grant and installed the mapping).
        let got = unsafe { reg.read_volatile() };

        // 3. PASS only if the register reads the expected device magic.
        if got == MAGIC {
            0
        } else {
            FAIL_MAGIC
        }
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
