//! The x86_64 (Multiboot2/ACPI PC) boot pipeline and `KernelArch` port
//! wrapper.
//!
//! Every module here names the x86_64 architecture port, so the whole
//! subtree is gated on the `kernel_isa = "x86_64"` build-script name (the
//! single selection point lives in `build.rs`, never an
//! inline `target_arch` predicate). The host-testable wrappers
//! ([`arch_wrapper`], [`dispatch`], [`ioapic_controller`], [`virtio_boot`],
//! [`driver_host`]) compile on the x86_64 CI host so their unit tests run
//! under `cargo test`; the bare-metal-only boot path, PID 1 spawn seam,
//! runtime spawn producer, panic bridge, and serial sink are further gated
//! on `freestanding`.

pub mod arch_wrapper;
pub mod dispatch;
pub mod driver_host;
pub mod ioapic_controller;
pub mod virtio_boot;

#[cfg(freestanding)]
pub mod boot;
#[cfg(freestanding)]
pub mod init_spawn;
#[cfg(freestanding)]
pub mod panic_ctx;
#[cfg(freestanding)]
pub mod serial_sink;
#[cfg(freestanding)]
pub mod spawn_producer;

/// First non-addressable user virtual address on this port.
///
/// The x86_64 port runs 4-level paging: a 48-bit virtual address space
/// whose canonical *lower* (user) half is `[0, 2^47)` — the same
/// `0x0000_8000_0000_0000` boundary the syscall-entry canonicality check
/// guards. `2^47` is therefore the first address a user mapping can never
/// reach, and the ceiling the dynamic heap and file-mapping windows
/// ([`crate::user_windows::user_windows`]) size themselves below so they can
/// never run past addressable user space. Genuinely target-specific — the
/// paging mode dictates it — so it lives beside the port, not in the
/// architecture-neutral layout module.
#[cfg(freestanding)]
pub const USER_VA_TOP: u64 = 1 << 47;
