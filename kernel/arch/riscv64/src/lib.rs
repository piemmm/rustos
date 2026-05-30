//! RustOS riscv64 architecture port.
//!
//! Stage 4.D Item 4 lands the QEMU `virt`-board boot pipeline up to
//! `AuditEvent::BootCompleted`: an S-mode entry trampoline, an SBI
//! console log sink, a minimal flattened-device-tree reader for the
//! physical-memory map and timer frequency, the [`RiscvArch`]
//! implementation of [`rustos_kernel_core::KernelArch`], and the
//! `boot()` entry that assembles a [`rustos_kernel_core::BootInfo`]
//! and hands it to `kernel_core::kernel_main`. (`boot` and the other
//! freestanding-only modules are gated to the riscv64 bare-metal
//! target, so this link is plain text on host doc builds.)
//!
//! # What is here
//!
//! | Module        | Role                                                            |
//! | ------------- | --------------------------------------------------------------- |
//! | [`fdt`]       | Flattened-device-tree reader (`/memory`, `timebase-frequency`). |
//! | [`kernel_arch`] | [`RiscvArch`] — the `KernelArch` impl + monotonic clock.      |
//! | [`plic`]      | PLIC driver + the `IrqController` the kernel masks through.     |
//! | [`publish`]   | Set-once boot-state slots (memory map + DTB pointer).           |
//! | [`trap`]      | S-mode trap vector + external-interrupt dispatch seam.          |
//! | [`qemu_exit`] | `SiFive` Test finisher used by the integration tests.           |
//! | `sbi`         | SBI legacy console output (freestanding only).                  |
//! | `serial`      | SBI-backed `rustos_log::Sink` (freestanding only).              |
//! | `entry`       | `rustos_arch_riscv64_main` Rust trampoline (freestanding only). |
//! | `boot`        | The `boot(hartid, dtb, …)` pipeline (freestanding only).        |
//! | `panic`       | Shared `#[panic_handler]` bridge (freestanding only).           |
//!
//! # Why depend on `kernel/core` here
//!
//! See `Cargo.toml`: unlike x86_64, this crate owns its `KernelArch`
//! impl and boot pipeline directly. The freestanding-only modules are
//! gated to `cfg(all(target_arch = "riscv64", target_os = "none"))`;
//! the [`fdt`] reader and the [`RiscvArch`] struct build on the host so
//! their unit tests run under `cargo test`.
//!
//! # Not yet here
//!
//! Sv39 paging, the ring-0 DTB virtio-mmio walk, and SMP bring-up — the
//! remaining riscv64 deliverables tracked in `PLAN.md` Stage 4.D
//! Item 4. The [`plic`] controller and the [`trap`] S-mode vector land
//! the external-IRQ foundation the virtio-mmio verticals build on; they
//! are not needed to reach `BootCompleted` (the `virt` board enters
//! S-mode with paging off and the init pipeline never faults), so the
//! boot pipeline does not arm them yet — the verticals will.
#![no_std]
#![deny(missing_docs)]

// Host unit tests use `std` (e.g. `std::vec::Vec` in the `fdt` fixture
// builder). The crate itself stays `no_std` for the freestanding
// build (`AGENTS.md` §1 — no hacks).
#[cfg(test)]
extern crate std;

// The boot pipeline builds an `Arc<RiscvArch>` for `BootInfo`, so it
// needs `alloc`. Only the freestanding build links it; the allocator
// is provided by the boot binary's `#[global_allocator]`.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
extern crate alloc;

// The S-mode entry trampoline is only meaningful on the bare-metal
// target; host `cargo test` omits it so the crate builds on the host.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
core::arch::global_asm!(include_str!("boot.s"));

pub mod fdt;
pub mod kernel_arch;
pub mod plic;
pub mod publish;
pub mod qemu_exit;
pub mod trap;

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub mod boot;
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub mod entry;
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub mod panic;
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub mod sbi;
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub mod serial;

pub use kernel_arch::RiscvArch;

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub use boot::{boot, BootError};
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub use panic::handle_panic_via_serial;
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub use serial::{SerialSink, SERIAL_SINK};
