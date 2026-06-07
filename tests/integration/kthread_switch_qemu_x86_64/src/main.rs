//! SPAWN Stage SP1 QEMU integration test: two kernel-thread tasks
//! ping-pong through the **real** Arch HAL `ContextSwitch::switch` under
//! the live `kernel/sched` scheduler on x86_64.
//!
//! ## What this test asserts
//!
//! `plans/SPAWN.md` Stage SP1 requires the `kernel/core` kthread runtime
//! to make a scheduler task a *resumable kernel thread* with its own
//! kernel stack, driven through the existing `ContextSwitch` HAL, and to
//! prove it on every bare-metal board by having **two** kthreads ping-pong
//! via the real `ContextSwitch::switch`. The aarch64 vertical landed first
//! (`tests/integration/kthread_switch_qemu_aarch64`); this is the x86_64
//! sibling, so the runtime is a *production* scheduling path on x86_64
//! too — until now `ContextSwitch::switch` was exercised here only by the
//! W7 context-switch conformance double.
//!
//! This binary exercises that on the multiboot-loaded kernel:
//!
//! 1. **Live scheduler over the production arch.** On the boot CPU it
//!    builds a real `rustos_kernel_sched_eevdf::Scheduler` over the
//!    production `rustos_arch_x86_64::kernel_arch::X86_64Arch` handle (read
//!    from the boot LAPIC id), with no AP bring-up and no timer armed —
//!    interrupts stay masked, so the cooperative `step` loop is the only
//!    dispatch mechanism and the kthread context switches are the only
//!    thing under test.
//! 2. **Two kthreads.** It spawns two kthreads through
//!    `rustos_kernel_core::spawn_kthread`. Each kthread body runs on its
//!    own kernel stack and calls `Yielder::yield_now` `PING_PONGS` times —
//!    each yield is a real `ContextSwitch::switch` back to the dispatcher —
//!    then returns (`Exit`).
//! 3. **Ping-pong + drain.** It drives the cooperative `step` loop until
//!    both kthreads have exited. PASS once both have run exactly
//!    `PING_PONGS` times and no task is left live.
//!
//! A regression in the runtime (a switch that does not transfer control, a
//! task that never resumes, a stack that is not reclaimed) either reports
//! a failure on the serial console and exits, or never drains the
//! workload, so the run fails loudly — by `qemu_exit::exit_failure` or by
//! the harness `Outcome::Timeout` (`AGENTS.md` §7).
//!
//! ## How it differs from a production kernel
//!
//! It links the `rustos-kernel-core` kthread runtime, the
//! `rustos-arch-x86_64` port, and the default `rustos-kernel-sched-eevdf`
//! policy directly and supplies its own `kernel_main`, so the runtime is
//! exercised without the full `kernel_core::kernel_main` init pipeline. The
//! QEMU-exit shortcut lives in this dedicated bin, never behind a Cargo
//! feature on a library crate (`AGENTS.md` §5.4.5 — fail closed).

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_x86_64)]
mod kernel;

// --- Host stub -----------------------------------------------------
// The crate produces a meaningful artefact only for x86_64-unknown-none;
// on the host triple it has nothing to run.
#[cfg(not(itest_x86_64))]
fn main() {}
#[cfg(not(itest_x86_64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
