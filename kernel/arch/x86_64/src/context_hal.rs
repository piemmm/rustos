//! x86_64 context switch (`AGENTS.md` §17.2 "context switch").
//!
//! Implements the Arch HAL [`ContextSwitch`](rustos_arch_api::ContextSwitch)
//! surface for x86_64 over the bare-metal switch primitive in
//! [`crate::context`]. The HAL handle is the architecture-neutral face of
//! the task-switch path: it seeds a never-run task's first frame
//! ([`ContextSwitch::prepare`](rustos_arch_api::ContextSwitch::prepare))
//! and performs the switch
//! ([`ContextSwitch::switch`](rustos_arch_api::ContextSwitch::switch)). The
//! callee-saved save/restore lives in [`crate::context`]'s `context.s`
//! assembly — per-CPU register work with no architecture-neutral shape
//! (`AGENTS.md` §2.4) — so this module reinterprets the neutral
//! [`TaskContext`](rustos_arch_api::TaskContext) as the port's
//! [`crate::context::TaskCtx`] and forwards, keeping the switch invoke in
//! exactly one place (`AGENTS.md` §2.2).
//!
//! The neutral [`TaskContext`](rustos_arch_api::TaskContext) and the port's
//! [`crate::context::TaskCtx`] are both a single `#[repr(C)]` `u64` (the
//! kernel `rsp` at suspension), so the forward is a layout-identical
//! reinterpretation, not a copy; the const-assert below pins that equality.

use rustos_arch_api::{ContextSwitch, PrepareError, TaskContext, TaskEntry};

use crate::context::{self, TaskCtx};

/// x86_64 implementation of the Arch HAL context-switch surface.
///
/// Zero-sized: a task's saved state lives in the caller-owned
/// [`TaskContext`], not in the handle, exactly like the
/// [`crate::timer_hal::TimerHal`] and [`crate::userentry`] handles.
#[derive(Debug, Default, Clone, Copy)]
pub struct ContextSwitchHal;

impl ContextSwitchHal {
    /// Construct the x86_64 context-switch handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

/// The neutral save area and the port's `TaskCtx` must be the same
/// single-word layout for the pointer reinterpretation in
/// [`ContextSwitch::switch`] to be sound (`AGENTS.md` §2.2).
const _CONTEXT_LAYOUT_MATCHES: () = {
    assert!(core::mem::size_of::<TaskContext>() == core::mem::size_of::<TaskCtx>());
    assert!(core::mem::align_of::<TaskContext>() == core::mem::align_of::<TaskCtx>());
};

/// Map the port primitive's prepare error onto the neutral HAL error.
const fn map_prepare_error(err: context::PrepareError) -> PrepareError {
    match err {
        context::PrepareError::NullStack => PrepareError::NullStack,
        context::PrepareError::Misaligned => PrepareError::Misaligned,
        context::PrepareError::TooSmall => PrepareError::TooSmall,
    }
}

impl ContextSwitch for ContextSwitchHal {
    fn prepare(
        &self,
        ctx: &mut TaskContext,
        stack_top: u64,
        entry: TaskEntry,
        arg: usize,
    ) -> Result<(), PrepareError> {
        let mut native = TaskCtx::new();
        native
            .prepare(stack_top, entry, arg)
            .map_err(map_prepare_error)?;
        ctx.stack_pointer = native.rsp;
        Ok(())
    }

    unsafe fn switch(&self, prev: *mut TaskContext, next: *mut TaskContext) {
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        {
            // SAFETY: `TaskContext` and `TaskCtx` have identical
            // single-word `#[repr(C)]` layout (pinned by the const-assert
            // above), so the pointer reinterpretation is sound. The
            // caller upholds the `ContextSwitch::switch` contract
            // (non-null, aligned, runnable `next`, exclusive `prev`),
            // which is exactly `crate::context::switch`'s contract.
            unsafe {
                context::switch(prev.cast::<TaskCtx>(), next.cast::<TaskCtx>());
            }
        }
        #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
        {
            let _ = (prev, next);
            unreachable!("context switch is only meaningful on the x86_64 bare-metal target")
        }
    }

    unsafe fn enter_cooperative_park(&self) {
        // Balance the entry `swapgs` (`crate::syscall_entry`) before a user
        // task parks mid-handler: flip `%gs` back to the *between-handler*
        // convention (current GS = user value, `IA32_KERNEL_GS_BASE` = kernel
        // TLS) the dispatcher and `crate::userentry::enter_user` expect, so
        // the next ring-3 entry of a *different* task sees a balanced state
        // (`plans/PI.md` X2). The matching `leave_cooperative_park` flips it
        // back on resume.
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        // SAFETY: `swapgs` is privileged and runs in ring 0 here, on the
        // running user task's own syscall-handler control flow, exactly once
        // before its park (the trait contract). It touches no memory and no
        // flags, only the GS-base/`KERNEL_GS_BASE` swap.
        unsafe {
            core::arch::asm!("swapgs", options(nomem, nostack, preserves_flags));
        }
    }

    unsafe fn leave_cooperative_park(&self) {
        // Inverse of `enter_cooperative_park`: re-establish the *in-handler*
        // convention (current GS = kernel TLS) the parked syscall handler
        // resumes into, so its `gs:`-relative accesses and the stub's exit
        // `swapgs` remain balanced (`plans/PI.md` X2).
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        // SAFETY: as `enter_cooperative_park`, paired with the prior call on
        // the same task's control flow on resume from its cooperative park.
        unsafe {
            core::arch::asm!("swapgs", options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_arch_api::context::conformance;

    #[test]
    fn passes_context_switch_conformance() {
        conformance::run_all(&ContextSwitchHal::new());
        let dynamic: &dyn ContextSwitch = &ContextSwitchHal::new();
        conformance::run_all(dynamic);
    }
}
