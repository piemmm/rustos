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
//! | [`paging`]    | Sv39 page-table primitives (PTE/VPN/`satp`, identity map, `switch`). |
//! | [`context`]   | Context-switch primitive (`TaskCtx` + `switch`).                |
//! | [`fault`]     | Set-once synchronous-exception (page-fault) handler hook.       |
//! | [`preempt`]   | Supervisor-timer scheduler-tick callback + SBI-timer arming.    |
//! | [`syscall_entry`] | `ecall` argument marshalling + dispatch callback.           |
//! | [`plic`]      | PLIC register driver (inherent mask/arm/claim).                 |
//! | [`trap`]      | S-mode trap vector + ecall/timer/external dispatch.             |
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
//! # Stage 3 architecture primitives
//!
//! [`paging`] (Sv39), [`context`] (context switch), [`preempt`]
//! (supervisor-timer preemption), and [`syscall_entry`] (`ecall`) are
//! the Stage-3 per-sub-stage arch primitives. Each keeps its pure
//! bit/encoding/layout math host-testable and gates only the
//! CSR/assembly operations to the freestanding target. The
//! `tests/integration/timer_preempt_qemu_riscv64` QEMU vertical proves
//! the timer path drives the scheduler-tick callback end-to-end.
//!
//! # Not yet here
//!
//! Multi-hart SMP bring-up (and the SBI `IPI` delivery the
//! `RiscvArch::send_ipi` no-op documents) and wiring the new
//! [`paging::AddressSpace`] / [`context`] switch into the live
//! scheduler remain riscv64 follow-ups. The
//! `tests/integration/memory_isolation_qemu_riscv64` QEMU vertical
//! proves the [`paging::AddressSpace`] / [`fault`] path enforces
//! hardware memory isolation (an attacker space faults on a victim-only
//! address). The [`plic`] controller and the [`trap`] S-mode vector
//! land the external-IRQ foundation the virtio-mmio verticals build on;
//! the boot-to-`BootCompleted` slice runs with paging off on a single
//! hart and does not arm them — the verticals do.
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

// The context-switch primitive's body is assembly defined only on the
// bare-metal target; host `cargo test` exercises the layout/`prepare`
// math without it.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
core::arch::global_asm!(include_str!("context.s"));

pub mod context;
pub mod fault;
pub mod fdt;
pub mod kernel_arch;
pub mod paging;
pub mod plic;
pub mod preempt;
pub mod qemu_exit;
pub mod syscall_entry;
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
