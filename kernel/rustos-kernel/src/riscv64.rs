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
pub mod spawn_producer;
