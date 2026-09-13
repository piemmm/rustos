//! The architecture-neutral trap callbacks every port's preempt slots hold.
//!
//! Each port's wiring module installs these into its own slots, and which
//! slots exist is the only thing that varies: x86_64 delivers its placement
//! IPI on a dedicated vector and so carries no separate reschedule slot. The
//! callbacks are the same `tairix_kernel_core::traps` entry points on every
//! target, so they are coerced here once rather than restated per port.
//!
//! Coercing once is also what makes the wiring tests sound: two coercions of
//! one `fn` item are not guaranteed to share an address, so a test comparing a
//! loaded slot against its own fresh coercion can fail on a correctly
//! installed callback. Every installer and every test reads the pointer from
//! here.

use tairix_arch_api::CpuId;

/// The EL0 / U-mode / ring-3 preemption point.
pub static PREEMPT_CALLBACK: extern "C" fn(CpuId) = tairix_kernel_core::on_user_preempt_point;
/// The per-port timer tick.
pub static TIMER_CALLBACK: extern "C" fn(CpuId) = tairix_kernel_core::on_timer_tick;
/// The reschedule IPI, for the ports that carry a slot of its own for it.
pub static IPI_CALLBACK: extern "C" fn(CpuId) = tairix_kernel_core::on_reschedule_ipi;

/// Whether a port's slot holds exactly `want`.
///
/// Both sides come from one coercion — `want` is a `static` above — so this
/// compares the pointer the install actually stored.
#[cfg(test)]
pub fn installed(slot: Option<extern "C" fn(CpuId)>, want: extern "C" fn(CpuId)) -> bool {
    slot.is_some_and(|got| core::ptr::fn_addr_eq(got, want))
}
