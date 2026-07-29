//! TAIRiX riscv64 architecture port.
//!
//! Stage 4.D Item 4 lands the QEMU `virt`-board external-IRQ and boot
//! primitives: an S-mode entry trampoline, an SBI console log sink, a
//! minimal flattened-device-tree reader for the physical-memory map
//! and timer frequency, the [`RiscvArch`] implementation of the Arch
//! HAL ([`tairix_arch_api::SchedulerArch`]), and the PLIC + S-mode trap
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
//! | `serial`      | SBI-backed `tairix_log::Sink` (freestanding only).              |
//! | `entry`       | `tairix_arch_riscv64_main` Rust trampoline (freestanding only). |
//! | `panic`       | Shared `#[panic_handler]` bridge (freestanding only).           |
//!
//! # Arch HAL boundary
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
// build (no hacks).
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

// The machine-takeover stack-switch trampoline and relocatable
// kernel-image self-test + reset stub (`plans/NEW-SUPERVISOR.md` §9) are
// meaningful only on the bare-metal target; the whole `takeover` module is
// freestanding-only (it is all CSR/assembly and destroys the machine, so it
// is proven by the memtest-takeover QEMU vertical, never a host test).
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
core::arch::global_asm!(include_str!("takeover.s"));

pub mod backtrace;
pub mod context;
/// riscv64 implementation of the Arch HAL context-switch surface
/// ([`tairix_arch_api::ContextSwitch`]): the
/// architecture-neutral first-frame seeding + task switch over the
/// bare-metal primitive in [`context`].
pub mod context_hal;
/// riscv64 implementation of the Arch HAL core-clock surface
/// ([`tairix_arch_api::CoreClock`]): the `rdcycle` core-clock counter
/// (core cycles, DVFS-tracking) over the `rdtime`/`timebase-frequency`
/// fixed reference, from which the kernel derives the live "cpu MHz".
pub mod coreclock;
/// riscv64 implementation of the Arch HAL CPU-feature-detection and
/// cycle-counter surfaces ([`tairix_arch_api::CpuFeatures`] /
/// [`tairix_arch_api::CpuCycles`]): the `misa` vector-extension bit, the
/// device-tree `riscv,isa`-string multi-letter extension decode, and the
/// `time`-CSR cycle counter the `lib/cpuops` dispatch framework gates and
/// benchmarks on.
pub mod cpufeatures;
/// Boot-CPU model-name discovery: the device-tree cpu `compatible` →
/// marketing-name mapping (`SiFive U74-MC`, …) the boot facts report.
pub mod cpuname;
/// riscv64 implementation of the Arch HAL platform-entropy surface
/// ([`tairix_arch_api::PlatformEntropy`]): the `Zkr` `seed` CSR source,
/// honestly `Pending` on the M-mode delegation (see the module docs).
pub mod entropy;
pub mod fault;
pub mod fdt;
pub mod kernel_arch;
/// riscv64 implementation of the Arch HAL memory-tagging surface
/// ([`tairix_arch_api::MemoryTagging`]). The RISC-V
/// cores TAIRiX targets implement no ratified memory-tagging extension,
/// so the port declares it an honest `Unsupported` (see the module docs).
pub mod memtag;
pub mod paging;
/// riscv64 implementation of the Arch HAL per-CPU storage surface
/// ([`tairix_arch_api::PerCpu`]): the `tp`
/// (thread-pointer) register read/write the per-hart anchor is reached
/// through.
pub mod percpu_hal;
/// riscv64 implementation of the Arch HAL early-boot platform-discovery
/// surface ([`tairix_arch_api::PlatformDiscovery`]): the FDT → [`tairix_abi::hwtree`] normalisation built on the
/// [`fdt`] reader.
pub mod platform;
pub mod plic;
pub mod preempt;
pub mod qemu_exit;
// The SBI module's `ecall` wrappers are individually gated to the
// freestanding target; its extension-id constants, `SbiRet`, and the
// `hart_mask_for` helper build on the host so their unit tests run
// under `cargo test`.
pub mod sbi;
/// riscv64 implementation of the Arch HAL side-channel mitigation
/// surface ([`tairix_arch_api::SideChannelMitigation`]).
pub mod sidechannel;
pub mod smp;
pub mod syscall_entry;
/// riscv64 implementation of the Arch HAL timer-programming surface
/// ([`tairix_arch_api::Timer`]): the architecture-
/// neutral scheduler-tick callback install + dispatch over the
/// supervisor (SBI) timer wired in [`preempt`].
pub mod timer_hal;
pub mod trap;
/// riscv64 fault-windowed user-copy routine backing the Arch HAL
/// guarded-copy slot (`tairix_arch_api::uaccess`): a kernel-mode page
/// fault taken inside the copy resumes at its fix-up and surfaces as an
/// error instead of the fatal path.
pub mod uaccess;
/// riscv64 implementation of the Arch HAL "enter user mode" surface
/// ([`tairix_arch_api::EnterUser`]): the one `sret`
/// sequence that drops a built process image into U-mode.
pub mod userentry;

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub mod entry;
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub mod panic;
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub mod serial;
/// riscv64 machine-takeover mechanism (`plans/NEW-SUPERVISOR.md` §9): the
/// Arch HAL [`tairix_arch_api::MachineTakeover`] body driving the pre-boot
/// Supervisor's one-way whole-RAM `memtest`. Freestanding
/// only — it is all CSR/assembly and destroys the machine, so it is proven
/// by the memtest-takeover QEMU vertical, never a host test.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub mod takeover;

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub use kernel_arch::halt_current_hart;
pub use kernel_arch::{RiscvArch, RiscvArchStorage};

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub use panic::handle_panic_via_serial;
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub use serial::{SerialSink, SERIAL_SINK};
