//! The riscv64 (QEMU `virt` / SiFive) boot pipeline and `KernelArch` port
//! wrapper.
//!
//! Every module here names the riscv64 architecture port, so the whole
//! subtree is gated on the `kernel_isa = "riscv64"` build-script name (the
//! single selection point lives in `build.rs`, never an
//! inline `target_arch` predicate). The host-testable [`dispatch`] wrapper
//! compiles on a riscv64 host so its unit tests run under `cargo test`; the
//! bare-metal-only boot path, PID 1 spawn seam, and runtime spawn producer
//! are further gated on `freestanding`.

pub mod dispatch;

#[cfg(freestanding)]
pub mod boot;
#[cfg(freestanding)]
pub mod init_spawn;
#[cfg(freestanding)]
pub mod irq;
#[cfg(freestanding)]
pub mod panic_ctx;
#[cfg(freestanding)]
pub mod root_unlock;
#[cfg(freestanding)]
pub mod spawn_producer;

/// First non-addressable user virtual address on this port.
///
/// The riscv64 ports run Sv39: a 39-bit virtual address whose bits 63:39
/// must sign-extend bit 38, so the canonical *lower* (user) half is
/// `[0, 2^38)` = 256 GiB. `2^38` is therefore the first address a user
/// mapping can never reach, and the ceiling the dynamic heap and
/// file-mapping windows ([`crate::user_windows::user_windows`]) size
/// themselves below so they can
/// never run past addressable user space. Genuinely target-specific — the
/// paging mode dictates it — so it lives beside the port, not in the
/// architecture-neutral layout module.
#[cfg(freestanding)]
pub const USER_VA_TOP: u64 = 1 << 38;
