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
//!    exit code `0` as PASS and any other as a failure.
//!
//! It is a **pure-Rust** program: it links the Rust userland
//! runtime `rustos-rt`, never the C ABI (`crt0` + `abi-sys`), which exists
//! solely for non-Rust programs. It is built
//! position-independent and converted to an `rxe` blob by the consuming test's
//! build script. On the host it is an inert stub so
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
    /// single source of truth, else the default).
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
    /// Exit code: the non-`FIXED` `mem_map` returned an error — the kernel's
    /// placement allocator refused to choose a base (`plans/PI.md`
    /// 5d-0-ii (c)).
    const FAIL_MEM_MAP: i32 = 13;
    /// Exit code: a value written into the placed region did not read back —
    /// the placed anonymous pages are not genuine, writable RAM.
    const FAIL_MEM_RW: i32 = 14;
    /// Exit code: `mem_unmap` of the placed region returned an error.
    const FAIL_MEM_UNMAP: i32 = 15;
    /// Exit code: `dma_alloc` returned an error — the kernel refused to carve
    /// a coherent DMA buffer (`plans/PI.md` 5d-0-ii (c) DMA half).
    const FAIL_DMA_ALLOC: i32 = 16;
    /// Exit code: a value written into the DMA buffer did not read back — the
    /// carved coherent region is not genuine, writable RAM.
    const FAIL_DMA_RW: i32 = 17;

    /// Bytes the non-`FIXED` `mem_map` round-trip requests (two pages).
    const MEM_MAP_LEN: usize = 2 * 4096;
    /// A recognisable sentinel written into the placed region and read back.
    const MEM_SENTINEL: u64 = 0x5055_4D50_5F4F_4B21;

    /// The device-resource grant handle the program carves its DMA buffer
    /// against. The mmio window is grant `1`; this is the second grant the
    /// vertical mints for the task (handles are monotonic from `1`).
    const DMA_GRANT_HANDLE: u64 = 2;
    /// Bytes the DMA-buffer round-trip requests (two pages).
    const DMA_ALLOC_LEN: usize = 2 * 4096;
    /// A recognisable sentinel written into the DMA buffer and read back.
    const DMA_SENTINEL: u64 = 0x444D_4100_5F4F_4B21;

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

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall — so a
    /// `0` return is observed by the kernel as `exit(0)` (PASS) and any other
    /// as a failure code.
    fn main() -> i32 {
        // 1. Map the sub-region of the granted device window that covers the
        //    register under test — from offset 0 through `REG_OFFSET + 4` —
        //    by handle. Mapping a bounded sub-region (not the whole grant) is
        //    the production contract; `mmio_map` returns
        //    the base VA of that sub-region. A negative result is the
        //    `-errno` the kernel returned (a refused or unresolved grant, or
        //    a sub-region escaping it).
        let base = rustos_rt::mmio_map(GRANT_HANDLE, 0, (REG_OFFSET + 4) as usize);
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
        //    in-bounds pointer (the kernel validated the
        //    grant and installed the mapping).
        let got = unsafe { reg.read_volatile() };

        // 3. The register must read the expected device magic.
        if got != MAGIC {
            return FAIL_MAGIC;
        }

        // 4. Exercise the non-`FIXED` `mem_map` placement allocator
        //    (`plans/PI.md` 5d-0-ii (c)): ask the kernel to choose a base for
        //    two anonymous pages, prove they are genuine writable RAM by
        //    round-tripping a sentinel, then release them.
        let placed = rustos_rt::mem_map(MEM_MAP_LEN, rustos_abi::MapFlags::empty(), 0);
        if placed < 0 {
            return FAIL_MEM_MAP;
        }
        #[allow(clippy::cast_sign_loss)] // `placed >= 0` checked above; it is a user VA.
        let cell = placed as u64 as *mut u64;
        // SAFETY: `mem_map` returned the base of `MEM_MAP_LEN` bytes of mapped,
        //    zeroed, USER-writable anonymous memory in this process's own
        //    address space, so `cell` is a valid, writable, in-bounds pointer
        //    (the kernel installed the mapping). The write
        //    is `volatile` so it is not elided before the read-back.
        let read_back = unsafe {
            cell.write_volatile(MEM_SENTINEL);
            cell.read_volatile()
        };
        if read_back != MEM_SENTINEL {
            return FAIL_MEM_RW;
        }
        if rustos_rt::mem_unmap(placed as u64, MEM_MAP_LEN) < 0 {
            return FAIL_MEM_UNMAP;
        }

        // 5. Exercise the `dma_alloc` carve (`plans/PI.md` 5d-0-ii (c) DMA
        //    half): carve a coherent DMA buffer against the granted DMA
        //    constraint and prove it is genuine writable RAM by round-tripping
        //    a sentinel through its CPU virtual base. `device` receives the
        //    device-visible base (unused here — the device-address copy-out is
        //    host-proven; this vertical proves the carve mechanism on metal).
        let mut device: u64 = 0;
        let dma = rustos_rt::dma_alloc(DMA_GRANT_HANDLE, DMA_ALLOC_LEN, &mut device);
        if dma < 0 {
            return FAIL_DMA_ALLOC;
        }
        #[allow(clippy::cast_sign_loss)] // `dma >= 0` checked above; it is a user VA.
        let dma_cell = dma as u64 as *mut u64;
        // SAFETY: `dma_alloc` returned the base of `DMA_ALLOC_LEN` bytes of
        //    mapped, zeroed, USER-writable coherent DMA memory in this
        //    process's own address space, so `dma_cell` is a valid, writable,
        //    in-bounds pointer (the kernel installed the
        //    mapping). The write is `volatile` so it is not elided before the
        //    read-back.
        let dma_read_back = unsafe {
            dma_cell.write_volatile(DMA_SENTINEL);
            dma_cell.read_volatile()
        };
        if dma_read_back != DMA_SENTINEL {
            return FAIL_DMA_RW;
        }

        // PASS: the granted window mapped and read its magic, a placed
        // anonymous region round-tripped and released, and a coherent DMA
        // buffer carved and round-tripped.
        0
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
