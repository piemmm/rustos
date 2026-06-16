//! The aarch64 (Raspberry Pi 4) boot pipeline and `KernelArch` port
//! wrapper.
//!
//! Every module here names the aarch64 architecture port, so the whole
//! subtree is gated on the `kernel_isa = "aarch64"` build-script name (the
//! single `AGENTS.md` §17.2 selection point lives in `build.rs`, never an
//! inline `target_arch` predicate). The host-testable wrappers
//! ([`arch_wrapper`], [`dispatch`]) compile on an aarch64 host so their unit
//! tests run under `cargo test`; the bare-metal-only boot path, PID 1 spawn
//! seam, and runtime spawn producer are further gated on `freestanding`.

pub mod arch_wrapper;
pub mod dispatch;

#[cfg(freestanding)]
pub mod boot;
#[cfg(freestanding)]
pub mod init_spawn;
#[cfg(freestanding)]
pub mod spawn_producer;
