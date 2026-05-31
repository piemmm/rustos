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
//! | [`smp`]       | Multi-hart bring-up: `tp` hart identity, SBI HSM hart start.     |
//! | [`plic`]      | PLIC register driver (inherent mask/arm/claim).                 |
//! | [`trap`]      | S-mode trap vector + ecall/timer/software/external dispatch.     |
//! | [`qemu_exit`] | `SiFive` Test finisher used by the integration tests.           |
//! | [`sbi`]       | SBI calls: console/timer (legacy), IPI + HSM hart start (v0.2).  |
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
//! (supervisor-timer preemption), [`syscall_entry`] (`ecall`), and
//! [`smp`] (multi-hart bring-up + IPI delivery) are the Stage-3 arch
//! primitives. Each keeps its pure bit/encoding/layout math
//! host-testable and gates only the CSR/assembly operations to the
//! freestanding target. The `tests/integration/timer_preempt_qemu_riscv64`
//! QEMU vertical proves the timer path drives the scheduler-tick
//! callback end-to-end, and `tests/integration/ipi_smp_qemu_riscv64`
//! proves the boot hart starts a second hart (SBI HSM) and delivers it
//! a directed IPI (SBI IPI extension → `sip.SSIP` trap → callback).
//!
//! # Multi-hart SMP
//!
//! [`smp`] owns secondary-hart start (SBI HSM `hart_start` via the
//! `smp.s` trampoline, which seeds each hart's `tp` and stack) and the
//! `tp`-derived hart identity [`RiscvArch`] reverse-maps to a dense
//! `CpuId`. `RiscvArch`'s `send_ipi` raises a supervisor software
//! interrupt on the target hart through the SBI IPI extension; the
//! `preempt` software-interrupt handler acknowledges (`sip.SSIP`) and
//! runs the IPI callback. Per-hart timer intervals live in [`preempt`]'s
//! per-hart slots.
//!
//! # Not yet here
//!
//! Wiring the [`paging::AddressSpace`] / [`context`] switch and the
//! `preempt`/`smp` primitives into the *live* `kernel/sched` scheduler
//! (so a real `Scheduler` drives `on_timer_tick` and a real task switch
//! runs) remains a riscv64 follow-up. The
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

// The secondary-hart entry trampoline (`smp.s`) is meaningful only on
// the bare-metal target; host `cargo test` exercises the `smp` module's
// pure hart-id/callback logic without it.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
core::arch::global_asm!(include_str!("smp.s"));

pub mod context;
pub mod fault;
pub mod fdt;
pub mod kernel_arch;
pub mod paging;
pub mod plic;
pub mod preempt;
pub mod qemu_exit;
// The SBI module's `ecall` wrappers are individually gated to the
// freestanding target; its extension-id constants, `SbiRet`, and the
// `hart_mask_for` helper build on the host so their unit tests run
// under `cargo test`.
pub mod sbi;
/// riscv64 implementation of the Arch HAL side-channel mitigation
/// surface ([`rustos_arch_api::SideChannelMitigation`], `AGENTS.md`
/// §19.1).
pub mod sidechannel;
pub mod smp;
pub mod syscall_entry;
pub mod trap;

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub mod entry;
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub mod panic;
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub mod serial;

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub use kernel_arch::halt_current_hart;
pub use kernel_arch::RiscvArch;

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub use panic::handle_panic_via_serial;
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub use serial::{SerialSink, SERIAL_SINK};
