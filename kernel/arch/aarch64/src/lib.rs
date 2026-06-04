//! RustOS aarch64 architecture port.
//!
//! Stage 3b delivers the QEMU `virt`-board boot and Arch-HAL primitives
//! for the 64-bit Arm port: an EL1 boot trampoline (with an EL2→EL1
//! drop), a PL011 UART console, the [`Aarch64Arch`] implementation of the
//! Arch HAL ([`rustos_arch_api::SchedulerArch`]), the EL1 exception
//! vector table, a GICv2 driver, generic-timer preemption, the stage-1
//! MMU primitives, the `svc` syscall-entry marshalling, and the ARM
//! semihosting test finisher. (The freestanding-only modules are gated to
//! the aarch64 bare-metal target, so their links are plain text on host
//! doc builds.)
//!
//! # What is here
//!
//! | Module        | Role                                                            |
//! | ------------- | --------------------------------------------------------------- |
//! | [`kernel_arch`] | [`Aarch64Arch`] — the `SchedulerArch` impl + `CNTPCT` clock.  |
//! | [`paging`]    | Stage-1 page-table primitives (descriptors, identity map, `switch`). |
//! | [`context`]   | Context-switch primitive (`TaskCtx` + `switch`).                |
//! | [`fault`]     | Set-once synchronous-exception (abort) handler hook.            |
//! | [`exceptions`] | EL1 vector install + IRQ / sync dispatch.                      |
//! | [`gic`]       | GICv2 distributor / CPU-interface / SGI driver.                 |
//! | [`preempt`]   | EL1 physical-timer scheduler-tick callback + arming.            |
//! | [`syscall_entry`] | `svc` argument marshalling + dispatch callback.             |
//! | [`qemu_exit`] | ARM semihosting `SYS_EXIT` finisher used by the integration tests. |
//! | `serial`      | PL011-backed `rustos_log::Sink` (freestanding only).            |
//! | `entry`       | `rustos_arch_aarch64_main` Rust trampoline (freestanding only). |
//! | `panic`       | Shared `#[panic_handler]` bridge (freestanding only).           |
//!
//! # Arch HAL boundary (`AGENTS.md` §17.2 / §17.4)
//!
//! Like x86_64 and riscv64, this crate is a pure Arch HAL
//! implementation: it names only `kernel/arch/api` and `lib/*`, never a
//! concrete kernel subsystem. The downstream boot consumer wraps
//! [`Aarch64Arch`] in its own `KernelArch` type. The freestanding-only
//! modules are gated to `cfg(all(target_arch = "aarch64", target_os =
//! "none"))`; the [`Aarch64Arch`] struct and every pure
//! bit/encoding/layout helper build on the host so their unit tests run
//! under `cargo test`.
//!
//! # Stage 3 architecture primitives
//!
//! [`paging`] (stage-1 MMU), [`context`] (context switch), [`preempt`]
//! (generic-timer preemption), and [`syscall_entry`] (`svc`) are the
//! Stage-3 arch primitives. Each keeps its pure bit/encoding/layout math
//! host-testable and gates only the system-register/assembly operations
//! to the freestanding target. The `tests/integration` QEMU verticals
//! prove the boot, timer-preemption, and memory-isolation paths
//! end-to-end on the `virt` board.
#![no_std]
#![deny(missing_docs)]

// Host unit tests use `std` where convenient; the crate itself stays
// `no_std` for the freestanding build (`AGENTS.md` §1 — no hacks).
#[cfg(test)]
extern crate std;

// The EL1 boot trampoline is only meaningful on the bare-metal target;
// host `cargo test` omits it so the crate builds on the host.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
core::arch::global_asm!(include_str!("boot.s"));

// The context-switch primitive's body is assembly defined only on the
// bare-metal target; host `cargo test` exercises the layout/`prepare`
// math without it.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
core::arch::global_asm!(include_str!("context.s"));

// The EL1 exception vector table is meaningful only on the bare-metal
// target; host `cargo test` exercises the `exceptions` module's pure
// kind-classification logic without it.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
core::arch::global_asm!(include_str!("vectors.s"));

pub mod context;
pub mod exceptions;
pub mod fault;
pub mod gic;
pub mod kernel_arch;
/// aarch64 implementation of the Arch HAL memory-tagging surface
/// ([`rustos_arch_api::MemoryTagging`], `AGENTS.md` §19.10) — Arm MTE
/// (`stg` store sequence, 16-byte / 4-bit tag granule); both slots are
/// honestly `Pending` on the Stage 6 MTE enable (see the module docs).
pub mod memtag;
pub mod paging;
pub mod preempt;
pub mod qemu_exit;
/// aarch64 implementation of the Arch HAL side-channel mitigation
/// surface ([`rustos_arch_api::SideChannelMitigation`], `AGENTS.md`
/// §19.1).
pub mod sidechannel;
pub mod syscall_entry;
/// aarch64 implementation of the Arch HAL "enter user mode" surface
/// ([`rustos_arch_api::EnterUser`], `AGENTS.md` §17.2): the one `eret`
/// sequence that drops a built process image into EL0.
pub mod userentry;

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod entry;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod panic;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod serial;

pub use kernel_arch::Aarch64Arch;

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub use kernel_arch::halt_current_cpu;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub use panic::handle_panic_via_serial;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub use serial::{SerialSink, SERIAL_SINK};
