//! TAIRiX aarch64 architecture port.
//!
//! Stage 3b delivers the QEMU `virt`-board boot and Arch-HAL primitives
//! for the 64-bit Arm port: an EL1 boot trampoline (with an EL2→EL1
//! drop), a PL011 UART console, the [`Aarch64Arch`] implementation of the
//! Arch HAL ([`tairix_arch_api::SchedulerArch`]), the EL1 exception
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
//! | [`el2`]       | Pinned EL2→EL1 hand-off register values the boot trampoline writes. |
//! | [`context`]   | Context-switch primitive (`TaskCtx` + `switch`).                |
//! | [`fault`]     | Set-once synchronous-exception (abort) handler hook.            |
//! | [`exceptions`] | EL1 vector install + IRQ / sync dispatch.                      |
//! | [`gic`]       | GICv2 distributor / CPU-interface / SGI driver.                 |
//! | [`preempt`]   | EL1 physical-timer scheduler-tick callback + arming.            |
//! | [`syscall_entry`] | `svc` argument marshalling + dispatch callback.             |
//! | [`qemu_exit`] | ARM semihosting `SYS_EXIT` finisher used by the integration tests. |
//! | `serial`      | PL011-backed `tairix_log::Sink` (freestanding only).            |
//! | `entry`       | `tairix_arch_aarch64_main` Rust trampoline (freestanding only). |
//! | `panic`       | Shared `#[panic_handler]` bridge (freestanding only).           |
//!
//! # Arch HAL boundary
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
// `no_std` for the freestanding build (no hacks).
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

// The secondary-core entry trampoline is meaningful only on the
// bare-metal target; host `cargo test` exercises the `smp` module's pure
// validity / decode logic without it.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
core::arch::global_asm!(include_str!("smp.s"));

/// BCM2711 PCIe root-complex MSI controller (Raspberry Pi 4): the
/// register-level driver for the root complex's internal MSI controller,
/// which demultiplexes the VL805 xHCI's message-signalled interrupts onto
/// one shared GIC SPI. Host-testable register/demux logic over an
/// [`brcm_msi::MsiMmio`] seam; the real MMIO is the freestanding
/// `brcm_msi::VolatileMsiMmio` over the discovered root-complex base.
pub mod brcm_msi;
/// Board-discovered console model + runtime MMIO base (`plans/PI.md` P2):
/// the one console abstraction ([`console::ConsoleModel`] — PL011 / BCM2835
/// AUX mini-UART register layouts) the freestanding `serial` sink
/// transmits through, configured from the firmware device tree
/// ([`console::configure_from_fdt`]). Host-testable; the MMIO accesses live
/// in the `serial` module.
pub mod console;
pub mod context;
/// aarch64 implementation of the Arch HAL context-switch surface
/// ([`tairix_arch_api::ContextSwitch`]): the
/// architecture-neutral first-frame seeding + task switch over the
/// bare-metal primitive in [`context`].
pub mod context_hal;
/// Boot-CPU model-name discovery: the `MIDR_EL1` implementer/part decode
/// (`ARM Cortex-A72`, …) the boot facts report.
pub mod cpuname;
/// Known EL2→EL1 hand-off register values (`HCR_EL2`, `CNTHCTL_EL2`,
/// `CPTR_EL2`, `MDCR_EL2`) the `boot.s` trampoline writes *whole* before
/// dropping to EL1 — the EL2 reset state is architecturally UNKNOWN on
/// real silicon and an UNKNOWN `HCR_EL2.TVM` hangs the MMU switch.
pub mod el2;
/// aarch64 implementation of the Arch HAL platform-entropy surface
/// ([`tairix_arch_api::PlatformEntropy`]): the ARMv8.5 `FEAT_RNG` `RNDR`
/// system register the kernel seeds its CSPRNG reserve from.
pub mod entropy;
pub mod exceptions;
pub mod fault;
/// aarch64 device-tree access: the aarch64-specific `virt`-board queries
/// (PSCI method, generic-timer PPI, `/memory`) layered on the shared
/// [`tairix_fdt`] parser.
pub mod fdt;
pub mod gic;
/// Heterogeneous (`big.LITTLE`) core classification: the pure
/// `capacity-dmips-mhz` → [`tairix_arch_api::CoreClass`] classifier
/// [`crate::kernel_arch::Aarch64Arch`] feeds from the device tree
/// (`plans/WIRING.md` Stage W10).
pub mod hetcore;
pub mod kernel_arch;
/// aarch64 implementation of the Arch HAL memory-tagging surface
/// ([`tairix_arch_api::MemoryTagging`]) — Arm MTE
/// (`stg` store sequence, 16-byte / 4-bit tag granule); both slots are
/// honestly `Pending` on the Stage 6 MTE enable (see the module docs).
pub mod memtag;
pub mod paging;
/// aarch64 implementation of the Arch HAL per-CPU storage surface
/// ([`tairix_arch_api::PerCpu`]): the `TPIDR_EL1`
/// system-register read/write the per-CPU anchor is reached through.
pub mod percpu_hal;
/// aarch64 implementation of the Arch HAL early-boot platform-discovery
/// surface ([`tairix_arch_api::PlatformDiscovery`]): the FDT → [`tairix_abi::hwtree`] normalisation built on the
/// [`fdt`] reader.
pub mod platform;
pub mod preempt;
/// PSCI (Power State Coordination Interface) firmware calls — the
/// `CPU_ON` secondary-core power-on path the SMP bring-up issues
/// (`plans/WIRING.md` W6).
pub mod psci;
pub mod qemu_exit;
/// aarch64 implementation of the Arch HAL side-channel mitigation
/// surface ([`tairix_arch_api::SideChannelMitigation`]).
pub mod sidechannel;
/// Multi-core (SMP) secondary-core bring-up primitives: the set-once
/// secondary entry, the `MPIDR_EL1` per-core identity read, and the PSCI
/// `CPU_ON` launcher (`plans/WIRING.md` Stage W6).
pub mod smp;
pub mod syscall_entry;
/// aarch64 implementation of the Arch HAL timer-programming surface
/// ([`tairix_arch_api::Timer`]): the architecture-
/// neutral scheduler-tick callback install + dispatch over the EL1
/// physical generic timer wired in [`preempt`].
pub mod timer_hal;
/// aarch64 fault-windowed user-copy routine backing the Arch HAL
/// guarded-copy slot (`tairix_arch_api::uaccess`): a same-EL data abort
/// taken inside the copy resumes at its fix-up and surfaces as an error
/// instead of the fatal path.
pub mod uaccess;
/// PL011 line and BCM2711 GPIO 14/15 pin-mux bring-up for the discovered
/// console: real Pi 4 silicon leaves `UART0` unrouted and disabled until
/// the kernel programs it (QEMU's PL011 powers up enabled, masking the
/// omission under emulation).
pub mod uart_init;
/// aarch64 implementation of the Arch HAL "enter user mode" surface
/// ([`tairix_arch_api::EnterUser`]): the one `eret`
/// sequence that drops a built process image into EL0.
pub mod userentry;
/// Framebuffer boot console (`plans/PI.md` P7b): console output defaults
/// to the attached display via the `VideoCore` firmware mailbox, with
/// the UART as the fallback when no display exists.
pub mod video;
/// aarch64 implementation of the Arch HAL lockup-watchdog surface
/// ([`tairix_arch_api::watchdog`]): the ~1 Hz virtual-timer (`CNTV`, PPI
/// 27) liveness-sample cadence that feeds the cross-CPU lockup detector,
/// plus the [`watchdog::Watchdog`] recovery handle (reschedule / attention
/// SGI). The MMIO/system-register programming is gated to the freestanding
/// target; the pure encoding/decoding (`SPSR` level, cadence, recovery
/// outcomes) is host-unit-tested.
pub mod watchdog;

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod entry;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod panic;
/// PL011 / mini-UART-backed `tairix_log::Sink` plus the buffered,
/// non-blocking serial transmit ring shared by the diagnostic log and the
/// `stream_write` console backing. The MMIO transmit/receive primitives
/// are gated to the bare-metal target (host stubs make them inert), so the
/// pure ring discipline and the buffered-drain decision logic are
/// host-unit-tested under `cargo test`.
pub mod serial;

pub use kernel_arch::{halt_current_cpu, Aarch64Arch, Aarch64ArchStorage};

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub use kernel_arch::enable_fp_el1;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub use panic::handle_panic_via_serial;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub use serial::{SerialSink, SERIAL_SINK};
