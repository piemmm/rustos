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
