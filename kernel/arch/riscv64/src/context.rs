//! riscv64 task context primitive.
//!
//! The riscv64 analogue of `kernel/arch/x86_64::context`. It defines
//! [`TaskCtx`] — the per-task register-save area the scheduler parks in
//! its task table — and `switch`, the bare-metal switch primitive. The
//! contract every architecture port owes `kernel/sched` is identical: a
//! stable `*mut TaskCtx` layout plus an `extern "C"` switch invoked at a
//! preemption / yield point.
//!
//! # Layout
//!
//! Only the kernel-stack pointer needs persisting in [`TaskCtx`]; the
//! callee-saved registers live on the outgoing task's stack in a fixed
//! prologue layout owned by `switch`. The RISC-V calling convention
//! ("RISC-V ABIs Specification") lists `ra` (x1) and `s0`–`s11`
//! (x8, x9, x18–x27) as the registers that must survive a call;
//! `switch` saves those plus the first argument register `a0` (x10) so
//! the first-run frame can deliver the task's argument.
//!
//! The `repr(C)` layout pins the field order so the assembly in
//! `context.s` can address the save slot by a fixed offset (`+0x00`).
//!
//! # Safety
//!
//! `switch` is `unsafe`. Every caller must uphold:
//!
//! * `prev` and `next` are non-null, properly aligned `*mut TaskCtx`s
//!   the kernel owns;
//! * `next.sp` is either zero (the task has never run — see
//!   [`TaskCtx::prepare`]) or a value `switch` previously wrote for that
//!   same task;
//! * the kernel stack referenced by `next.sp` is mapped, exclusive to
//!   this hart for the call, and 16-byte aligned.

use core::mem::size_of;

/// Per-task register-save area.
///
/// One [`TaskCtx`] per scheduler task. The only field is the kernel
/// stack pointer at the moment `switch` last suspended this task; the
/// callee-saved registers are persisted *on the stack itself*, in a
/// fixed layout owned by `switch`.
///
/// A freshly-constructed `TaskCtx` has `sp == 0`: callers that want a
/// task to run must seed an initial frame via [`TaskCtx::prepare`].
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TaskCtx {
    /// Kernel stack pointer at suspension. Read by the resume half of
    /// `switch`; written by the suspend half.
    pub sp: u64,
}

impl TaskCtx {
    /// Build an empty context. `sp` is zero; the task is not runnable
    /// until [`Self::prepare`] seeds a frame.
    #[must_use]
    pub const fn new() -> Self {
        Self { sp: 0 }
    }

    /// Seed an initial frame so the first `switch` *into* this task
    /// lands at `entry` with the first argument register `a0` set to
    /// `arg`.
    ///
    /// `stack_top` is the *exclusive* upper bound of the task's kernel
    /// stack (one byte past the last addressable byte). It must be
    /// 16-byte aligned (RISC-V ABI stack alignment) and non-zero.
    ///
    /// On success `self.sp` points at the bottom of the synthesised
    /// frame, whose layout matches the suspend epilogue of `switch`
    /// exactly so the first resume restores the zeroed callee-saved
    /// registers, loads `a0 = arg`, and `ret`s into `entry`.
    ///
    /// # Errors
    ///
    /// [`PrepareError::NullStack`] if `stack_top == 0`;
    /// [`PrepareError::Misaligned`] if `stack_top % 16 != 0`;
    /// [`PrepareError::TooSmall`] if `stack_top` has no room for the
    /// synthesised frame.
    pub fn prepare(
        &mut self,
        stack_top: u64,
        entry: unsafe extern "C" fn(usize) -> !,
        arg: usize,
    ) -> Result<(), PrepareError> {
        if stack_top == 0 {
            return Err(PrepareError::NullStack);
        }
        if stack_top % 16 != 0 {
            return Err(PrepareError::Misaligned);
        }
        if stack_top < FRAME_BYTES {
            return Err(PrepareError::TooSmall);
        }
        // Frame layout the resume half of `switch` expects to restore,
        // in ascending address order from `sp`:
        //
        //   [sp + 0x00]  ra   (return address, seeded to `entry`)
        //   [sp + 0x08]  s0   (callee-saved, seeded to 0)
        //   ...                (s1..s11, seeded to 0)
        //   [sp + 0x60]  s11
        //   [sp + 0x68]  a0   (first-run argument, seeded to `arg`)
        let sp = stack_top - FRAME_BYTES;
        // SAFETY: `stack_top` is non-zero, 16-byte aligned, and at least
        // `FRAME_BYTES` above zero by the checks above. The caller's
        // documented contract is that `[stack_top - stack_size, stack_top)`
        // is mapped, exclusive to this hart, and writable; the frame fits
        // entirely in the topmost `FRAME_BYTES` of that range.
        unsafe {
            let p = sp as *mut u64;
            // ra <- entry
            core::ptr::write(p, entry as usize as u64);
            // s0..s11 <- 0
            for i in 1..=12 {
                core::ptr::write(p.add(i), 0);
            }
            // a0 <- arg
            core::ptr::write(p.add(13), arg as u64);
        }
        self.sp = sp;
        Ok(())
    }
}

/// Errors returned by [`TaskCtx::prepare`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareError {
    /// `stack_top` was zero.
    NullStack,
    /// `stack_top` was not 16-byte aligned (RISC-V ABI).
    Misaligned,
    /// `stack_top` had no room for the initial frame.
    TooSmall,
}

/// Byte size of the initial resume frame [`TaskCtx::prepare`] writes:
/// fourteen 8-byte slots — `ra`, `s0`–`s11`, and `a0`. Kept in step
/// with the assembly in `context.s` by the const-asserts below; 112 is
/// a multiple of 16 so the stack stays ABI-aligned.
const FRAME_BYTES: u64 = 14 * 8;

/// Compile-time pinning of the [`TaskCtx`] layout. The `switch`
/// assembly addresses `TaskCtx::sp` by the constant offset `0x00`.
#[allow(dead_code)] // const-assert; never referenced at runtime.
const TASK_CTX_LAYOUT_PINNED: () = {
    assert!(size_of::<TaskCtx>() == 8);
    assert!(FRAME_BYTES % 16 == 0);
};

// --- Context switch primitive ---------------------------------------

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
extern "C" {
    /// Defined in `context.s` (included via `global_asm!` in
    /// [`crate::lib`]). The `extern "C"` declaration gives the symbol
    /// the correct ABI so callers pass `TaskCtx` pointers in `a0`/`a1`.
    pub fn tairix_arch_riscv64_switch(prev: *mut TaskCtx, next: *mut TaskCtx);
}

/// Switch from `prev` to `next` on the current hart.
///
/// Saves the calling task's callee-saved registers onto its kernel
/// stack, records the resulting `sp` in `*prev`, loads `(*next).sp`, and
/// restores the inbound task's saved registers. Control returns to the
/// call site of the *previous* `switch` for the inbound task — or, for a
/// never-run task whose `sp` was seeded by [`TaskCtx::prepare`], to that
/// task's `entry`.
///
/// # Safety
///
/// See the module-level safety contract. In summary the caller must
/// guarantee that `prev`/`next` are non-null, `prev` belongs to the
/// running task and is exclusive to this hart, `next.sp` is zero
/// (unreachable) or a value `switch`/[`TaskCtx::prepare`] wrote, and the
/// inbound kernel stack is mapped and exclusive to this hart.
///
/// `next.sp == 0` is a kernel bug — the resume half would load a zero
/// stack pointer. Callers must run a freshly-prepared task through
/// [`TaskCtx::prepare`] first.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub unsafe fn switch(prev: *mut TaskCtx, next: *mut TaskCtx) {
    // SAFETY: forwarded from the caller's contract. The assembly saves
    // ra/s0..s11/a0 to `*prev`'s stack, swaps `sp`, and restores from
    // `*next`'s stack.
    unsafe { tairix_arch_riscv64_switch(prev, next) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, offset_of};

    #[test]
    fn task_ctx_layout_is_fixed() {
        assert_eq!(size_of::<TaskCtx>(), 8);
        assert_eq!(align_of::<TaskCtx>(), 8);
        assert_eq!(offset_of!(TaskCtx, sp), 0);
    }

    #[test]
    fn task_ctx_new_is_zero() {
        assert_eq!(TaskCtx::new().sp, 0);
    }

    // Address-only `entry`; never invoked by the host tests. The body
    // diverges via `panic!` rather than `loop {}` so clippy's
    // `empty_loop` lint does not fire outside `no_std`.
    extern "C" fn host_entry(_arg: usize) -> ! {
        panic!("host_entry is address-only; never invoked")
    }

    #[test]
    fn prepare_rejects_null_stack() {
        let mut c = TaskCtx::new();
        assert_eq!(
            c.prepare(0, host_entry, 0).unwrap_err(),
            PrepareError::NullStack
        );
    }

    #[test]
    fn prepare_rejects_misaligned_stack() {
        let mut c = TaskCtx::new();
        assert_eq!(
            c.prepare(0x1_0001, host_entry, 0).unwrap_err(),
            PrepareError::Misaligned
        );
    }

    #[test]
    fn prepare_rejects_too_small_stack() {
        let mut c = TaskCtx::new();
        // 16-byte aligned, but below the 112-byte frame.
        assert_eq!(
            c.prepare(0x10, host_entry, 0).unwrap_err(),
            PrepareError::TooSmall
        );
    }

    #[test]
    fn prepare_writes_initial_frame() {
        #[repr(C, align(16))]
        struct Stack([u64; 16]);
        let mut stack = Stack([0xDEAD_BEEF_DEAD_BEEFu64; 16]);
        let top = unsafe { core::ptr::addr_of_mut!(stack.0).cast::<u64>().add(16) } as u64;
        let mut c = TaskCtx::new();
        c.prepare(top, host_entry, 0xCAFE).unwrap();
        assert_eq!(c.sp, top - FRAME_BYTES);
        let frame = unsafe { core::slice::from_raw_parts(c.sp as *const u64, 14) };
        // ra <- entry
        assert_eq!(frame[0], host_entry as *const () as usize as u64);
        // s0..s11 <- 0
        for slot in &frame[1..13] {
            assert_eq!(*slot, 0);
        }
        // a0 <- arg
        assert_eq!(frame[13], 0xCAFE);
    }
}
