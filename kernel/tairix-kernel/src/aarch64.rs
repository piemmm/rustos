//! The aarch64 (Raspberry Pi 4) boot pipeline and `KernelArch` port
//! wrapper.
//!
//! Every module here names the aarch64 architecture port, so the whole
//! subtree is gated on the `kernel_isa = "aarch64"` build-script name (the
//! single selection point lives in `build.rs`, never an
//! inline `target_arch` predicate). The host-testable wrappers
//! ([`arch_wrapper`], [`dispatch`]) compile on an aarch64 host so their unit
//! tests run under `cargo test`; the bare-metal-only boot path, PID 1 spawn
//! seam, and runtime spawn producer are further gated on `freestanding`.

pub mod arch_wrapper;
pub mod dispatch;
pub mod gic_irq;

#[cfg(freestanding)]
pub mod boot;
#[cfg(freestanding)]
pub mod init_spawn;
#[cfg(freestanding)]
pub mod panic_ctx;
#[cfg(freestanding)]
pub mod root_unlock;
#[cfg(freestanding)]
pub mod spawn_producer;

/// First non-addressable user virtual address on this port.
///
/// The aarch64 port configures `TCR_EL1.T0SZ = 25`, so the `TTBR0_EL1`
/// (user) translation regime covers a 39-bit virtual address range
/// `[0, 2^39)` = 512 GiB. `2^39` is therefore the first address a user
/// mapping can never reach, and the ceiling the dynamic heap and
/// file-mapping windows ([`crate::user_windows::user_windows`]) size
/// themselves below so they can
/// never run past addressable user space. Genuinely target-specific — the
/// `T0SZ` the port programs dictates it — so it lives beside the port, not
/// in the architecture-neutral layout module.
#[cfg(freestanding)]
pub const USER_VA_TOP: u64 = 1 << 39;
