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

use tairix_arch_api::{KernelStackRegion, PrepareError, STACK_ALIGN};

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
    /// `stack` is the task's usable kernel stack; the frame occupies its
    /// topmost `FRAME_BYTES`.
    ///
    /// On success `self.sp` points at the bottom of the synthesised
    /// frame, whose layout matches the suspend epilogue of `switch`
    /// exactly so the first resume restores the zeroed callee-saved
    /// registers, loads `a0 = arg`, and `ret`s into `entry`.
    ///
    /// # Errors
    ///
    /// [`PrepareError::Misaligned`] if the region's top is not 16-byte
    /// aligned (RISC-V ABI stack alignment); [`PrepareError::TooSmall`]
    /// if it has no room for the synthesised frame.
    pub fn prepare(
        &mut self,
        stack: KernelStackRegion,
        entry: unsafe extern "C" fn(usize) -> !,
        arg: usize,
    ) -> Result<(), PrepareError> {
        // Frame layout the resume half of `switch` expects to restore,
        // in ascending address order from `sp`:
        //
        //   [sp + 0x00]  ra   (return address, seeded to `entry`)
        //   [sp + 0x08]  s0   (callee-saved, seeded to 0)
        //   ...                (s1..s11, seeded to 0)
        //   [sp + 0x60]  s11
        //   [sp + 0x68]  a0   (first-run argument, seeded to `arg`)
        let frame = stack.seed_frame(FRAME_BYTES)?;
        let p = frame.cast::<u64>();
        // SAFETY: `seed_frame` returned `FRAME_BYTES` of the region, which
        // its constructor vouches is mapped, writable, and exclusive to
        // this task; the region's top is 16-byte aligned and `FRAME_BYTES`
        // is a multiple of 8, so `p` is aligned for the `u64` writes below
        // and every index stays inside the frame.
        unsafe {
            // ra <- entry
            p.write(entry as usize as u64);
            // s0..s11 <- 0
            for i in 1..=12 {
                p.add(i).write(0);
            }
            // a0 <- arg
            p.add(13).write(arg as u64);
        }
        self.sp = frame.addr().get() as u64;
        Ok(())
    }
}

/// Byte size of the initial resume frame [`TaskCtx::prepare`] writes:
/// fourteen 8-byte slots — `ra`, `s0`–`s11`, and `a0`. Kept in step
/// with the assembly in `context.s` by the const-asserts below; 112 is
/// a multiple of 16 so the stack stays ABI-aligned.
const FRAME_BYTES: usize = 14 * 8;

/// Compile-time pinning of the [`TaskCtx`] layout. The `switch`
/// assembly addresses `TaskCtx::sp` by the constant offset `0x00`.
#[allow(dead_code)] // const-assert; never referenced at runtime.
const TASK_CTX_LAYOUT_PINNED: () = {
    assert!(size_of::<TaskCtx>() == 8);
    assert!(FRAME_BYTES.is_multiple_of(STACK_ALIGN));
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
    use core::ptr::NonNull;

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

    /// A real, 16-byte-aligned stack buffer. The frame is asserted by
    /// reading *this* buffer back, so the test proves the write landed in
    /// the region rather than trusting the address `prepare` reported.
    #[repr(C, align(16))]
    struct Stack([u64; STACK_WORDS]);

    const STACK_WORDS: usize = 32;
    const STACK_BYTES: usize = STACK_WORDS * 8;

    impl Stack {
        fn new() -> Self {
            Self([0xDEAD_BEEF_DEAD_BEEF; STACK_WORDS])
        }

        /// The lowest `len` bytes of the buffer, as a region.
        fn region(&mut self, len: usize) -> KernelStackRegion {
            assert!(len <= STACK_BYTES);
            let ptr = NonNull::from(&mut self.0).cast::<u8>();
            // SAFETY: `ptr` addresses `len <= STACK_BYTES` bytes of this
            // live, uniquely borrowed buffer, which outlives the region.
            unsafe { KernelStackRegion::new(ptr, len) }
        }
    }

    /// Big enough for the frame, so only the unaligned top can refuse it.
    #[test]
    fn prepare_rejects_misaligned_stack() {
        let mut stack = Stack::new();
        let mut c = TaskCtx::new();
        assert_eq!(
            c.prepare(stack.region(STACK_BYTES - 8), host_entry, 0)
                .unwrap_err(),
            PrepareError::Misaligned
        );
        assert_eq!(c.sp, 0, "a refused prepare must leave the context unseeded");
    }

    /// 16-byte aligned, but below the 112-byte frame.
    #[test]
    fn prepare_rejects_too_small_stack() {
        let mut stack = Stack::new();
        let mut c = TaskCtx::new();
        assert_eq!(
            c.prepare(stack.region(16), host_entry, 0).unwrap_err(),
            PrepareError::TooSmall
        );
        assert_eq!(c.sp, 0, "a refused prepare must leave the context unseeded");
    }

    #[test]
    fn prepare_writes_initial_frame() {
        let mut stack = Stack::new();
        let region = stack.region(STACK_BYTES);
        let top = region.top_addr();
        let mut c = TaskCtx::new();
        // Coerce once: the frame word is compared against *this* pointer
        // value, because two coercions of one `fn` item are not
        // guaranteed to share an address.
        let entry: unsafe extern "C" fn(usize) -> ! = host_entry;
        c.prepare(region, entry, 0xCAFE).unwrap();
        assert_eq!(c.sp, top - FRAME_BYTES as u64);
        let frame = &stack.0[STACK_WORDS - FRAME_BYTES / 8..];
        // ra <- entry
        assert_eq!(frame[0], entry as *const () as usize as u64);
        // s0..s11 <- 0
        for slot in &frame[1..13] {
            assert_eq!(*slot, 0);
        }
        // a0 <- arg
        assert_eq!(frame[13], 0xCAFE);
    }
}
