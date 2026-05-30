//! RustOS riscv64 architecture port.
//!
//! Stage 4.D Item 4 lands the QEMU `virt`-board external-IRQ and boot
//! primitives: an S-mode entry trampoline, an SBI console log sink, a
//! minimal flattened-device-tree reader for the physical-memory map
//! and timer frequency, the [`RiscvArch`] implementation of the Arch
//! HAL ([`rustos_arch_api::SchedulerArch`]), and the PLIC + S-mode trap
//! vector. (The freestanding-only modules are gated to the riscv64
//! bare-metal target, so their links are plain text on host doc
//! builds.)
//!
//! # What is here
//!
//! | Module        | Role                                                            |
//! | ------------- | --------------------------------------------------------------- |
//! | [`fdt`]       | Flattened-device-tree reader (`/memory`, `timebase-frequency`). |
//! | [`kernel_arch`] | [`RiscvArch`] — the `SchedulerArch` impl + monotonic clock.   |
//! | [`plic`]      | PLIC register driver (inherent mask/arm/claim).                 |
//! | [`trap`]      | S-mode trap vector + external-interrupt dispatch seam.          |
//! | [`qemu_exit`] | `SiFive` Test finisher used by the integration tests.           |
//! | `sbi`         | SBI legacy console output (freestanding only).                  |
//! | `serial`      | SBI-backed `rustos_log::Sink` (freestanding only).              |
//! | `entry`       | `rustos_arch_riscv64_main` Rust trampoline (freestanding only). |
//! | `panic`       | Shared `#[panic_handler]` bridge (freestanding only).           |
//!
//! # Arch HAL boundary (`AGENTS.md` §17.2 / §17.4)
//!
//! Like x86_64, this crate is a pure Arch HAL implementation: it names
//! only `kernel/arch/api` and `lib/*`, never a concrete kernel
//! subsystem. The `KernelArch` wrapper, the `BootInfo` assembly
//! (`boot`), the set-once boot-state slots, and the `IrqController`
//! bridge over [`plic::PlicController`] all live in the downstream boot
//! consumer. The freestanding-only modules are gated to
//! `cfg(all(target_arch = "riscv64", target_os = "none"))`; the [`fdt`]
//! reader and the [`RiscvArch`] struct build on the host so their unit
//! tests run under `cargo test`.
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

// The S-mode entry trampoline is only meaningful on the bare-metal
// target; host `cargo test` omits it so the crate builds on the host.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
core::arch::global_asm!(include_str!("boot.s"));

pub mod fdt;
pub mod kernel_arch;
pub mod plic;
pub mod qemu_exit;
pub mod trap;

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub mod entry;
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub mod panic;
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub mod sbi;
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub mod serial;

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub use kernel_arch::halt_current_hart;
pub use kernel_arch::RiscvArch;

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub use panic::handle_panic_via_serial;
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub use serial::{SerialSink, SERIAL_SINK};
