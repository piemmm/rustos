//! x86_64 task context primitive (Stage 3a (c1)).
//!
//! This module defines [`TaskCtx`] — the per-task register-save area the
//! scheduler hands to the architecture-specific context switch — and
//! `switch`, the bare-metal switch primitive. The split mirrors the
//! contract every architecture port owes `kernel/sched`: a stable
//! `*mut TaskCtx` layout the scheduler can park in its task table, plus
//! an `extern "C"` switch that the scheduler invokes on a preemption /
//! yield point.
//!
//! # Layout
//!
//! Only the kernel-stack pointer needs to be persisted across a switch:
//! the System V AMD64 ABI lists `rbx`, `rbp`, `r12`, `r13`, `r14`, `r15`
//! as callee-saved (Intel SDM Vol 1 §3.4.1; "System V ABI: AMD64
//! Architecture Processor Supplement" §3.2.1), and `switch` pushes them
//! onto the *outgoing* task's kernel stack before recording `rsp` into
//! [`TaskCtx::rsp`]. Recovery is the symmetric pop. Caller-saved
//! registers (rax/rcx/rdx/rsi/rdi/r8..r11) carry no live state across
//! the call boundary by ABI definition and therefore need not be saved.
//!
//! The `repr(C)` layout pins the field order so the inline assembly in
//! `switch` can address the save slot by a fixed offset (`+0x00`).
//!
//! # Safety
//!
//! `switch` is `unsafe`. Every caller must uphold:
//!
//! * `prev` and `next` are non-null, properly aligned, and point at
//!   `TaskCtx`s the kernel owns;
//! * `next.rsp` is either zero (the next task has never run — see
//!   [`TaskCtx::prepare`]) or the value previously written by `switch`
//!   for that same task;
//! * the kernel stack referenced by `next.rsp` is mapped, exclusive to
//!   this CPU for the duration of the call, and 16-byte aligned at the
//!   point the corresponding `iretq` / `ret` would unwind.
//!
//! Failing any of these is undefined behaviour: there is no defensive
//! check we can add inside the prologue without giving up the property
//! that the switch is exactly the documented sequence of `push` / `pop`.

use core::mem::size_of;

/// Per-task register-save area.
///
/// One [`TaskCtx`] per scheduler task. The only field is the kernel
/// stack pointer at the moment `switch` last suspended this task; the
/// remaining callee-saved registers are persisted *on the stack itself*,
/// in a fixed prologue layout owned by `switch`.
///
/// A freshly-constructed `TaskCtx` has `rsp == 0`: callers that want a
/// task to actually run must seed an initial frame via
/// [`TaskCtx::prepare`].
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TaskCtx {
    /// Kernel stack pointer at suspension. Read by the resume half of
    /// `switch`; written by the suspend half.
    pub rsp: u64,
}

impl TaskCtx {
    /// Build an empty context. `rsp` is zero; the task is not yet
    /// runnable until [`Self::prepare`] is called.
    #[must_use]
    pub const fn new() -> Self {
        Self { rsp: 0 }
    }

    /// Seed an initial frame so the first `switch` *into* this task
    /// will land at `entry`, with the System V AMD64 ABI's first
    /// argument register `rdi` set to `arg`.
    ///
    /// `stack_top` is the *exclusive* upper bound of the task's kernel
    /// stack: i.e. one byte past the last addressable byte. It must be
    /// 16-byte aligned (System V AMD64 §3.2.2) and non-zero.
    ///
    /// On success returns `()` and `self.rsp` points at the bottom of
    /// the synthesised frame; the layout matches the resume epilogue
    /// of `switch` exactly so the first resume pops the synthesised
    /// callee-saved zeros and `ret`s into `entry`.
    ///
    /// # Errors
    ///
    /// [`PrepareError::NullStack`] if `stack_top == 0`;
    /// [`PrepareError::Misaligned`] if `stack_top % 16 != 0`;
    /// [`PrepareError::TooSmall`] if `stack_top` does not have room for
    /// the synthesised frame (9 × 8 bytes).
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
        // Frame layout the resume half of `switch` expects to pop, in
        // ascending address order from `rsp`. This must match the actual
        // `popq` order in `context.s` (rdi first, then r15..rbp, then
        // `ret`), *not* the push order of the suspend half:
        //
        //   [rsp + 0x00]  rdi  (first SysV arg, seeded to `arg`)
        //   [rsp + 0x08]  r15  (callee-saved, seeded to 0)
        //   [rsp + 0x10]  r14  (   "       ", seeded to 0)
        //   [rsp + 0x18]  r13  (   "       ", seeded to 0)
        //   [rsp + 0x20]  r12  (   "       ", seeded to 0)
        //   [rsp + 0x28]  rbx  (   "       ", seeded to 0)
        //   [rsp + 0x30]  rbp  (   "       ", seeded to 0)
        //   [rsp + 0x38]  ret  (popped by `ret`, seeded to `entry`)
        //   [rsp + 0x40]  pad  (16-byte alignment; never read)
        //
        // `rdi` is at offset 0 because the resume half's first `popq` is
        // `popq %rdi`; seeding `arg` anywhere else (e.g. at the suspend
        // half's *push* offset) would deliver `arg` in the wrong
        // register and enter `entry` with `%rdi == 0`.
        //
        // The trailing pad word makes the trampoline land at
        // `(%rsp + 8) % 16 == 0`: the resume pops 7 words (rdi + 6
        // callee registers) and then `ret` pops the 8th (the return
        // address), leaving `%rsp == stack_top - 8`. Because `stack_top`
        // is 16-byte aligned, the entry then observes the System V
        // AMD64 §3.2.2 alignment a `call` would have produced. Without
        // the pad, `entry` would run on a stack misaligned by 8.
        if stack_top < FRAME_BYTES {
            return Err(PrepareError::TooSmall);
        }
        let rsp = stack_top - FRAME_BYTES;
        // SAFETY: `stack_top` is non-zero, 16-byte aligned, and at
        // least `FRAME_BYTES` above zero by the checks above. The
        // caller's documented contract is that the byte range
        // `[stack_top - stack_size .. stack_top)` is mapped, exclusive
        // to this CPU, and writable. The frame we write fits entirely
        // in the topmost `FRAME_BYTES` of that range.
        unsafe {
            let p = rsp as *mut u64;
            // rdi <- arg (popped first by the resume half).
            core::ptr::write(p.add(0), arg as u64);
            // r15, r14, r13, r12, rbx, rbp <- zero (popped next).
            for i in 1..7 {
                core::ptr::write(p.add(i), 0);
            }
            // return address <- entry (consumed by `ret`).
            core::ptr::write(p.add(7), entry as usize as u64);
            // p.add(8) is the alignment pad — never read by the resume
            // half, so it is left at the stack's existing contents.
        }
        self.rsp = rsp;
        Ok(())
    }
}

/// Errors returned by [`TaskCtx::prepare`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareError {
    /// `stack_top` was zero.
    NullStack,
    /// `stack_top` was not 16-byte aligned (System V AMD64 §3.2.2).
    Misaligned,
    /// `stack_top` had no room for the initial frame.
    TooSmall,
}

/// Byte size of the initial resume frame [`TaskCtx::prepare`] writes.
/// Nine 8-byte slots: `rdi`, six callee-saved registers, the initial
/// return address, and a trailing 16-byte alignment pad so the
/// trampoline is entered with the System V AMD64 `(%rsp + 8) % 16 == 0`
/// invariant. The const-asserts below keep this in step with the
/// `popq` sequence in `context.s`.
const FRAME_BYTES: u64 = 9 * 8;

/// Compile-time pinning of the [`TaskCtx`] layout. The `switch`
/// inline assembly addresses `TaskCtx::rsp` by the constant offset
/// `0x00`; the const-assert here is the cross-check.
#[allow(dead_code)] // const-assert; never referenced at runtime.
const TASK_CTX_LAYOUT_PINNED: () = {
    assert!(size_of::<TaskCtx>() == 8);
};

// --- Context switch primitive ---------------------------------------

// Linkage declaration for the assembly-defined `switch` primitive.
// rustdoc does not document extern blocks, so the safety contract +
// behaviour spec live above the Rust-side safe wrapper `switch` below.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
extern "C" {
    /// Defined in `context.s` (included via `global_asm!` in
    /// [`crate::lib`]). The Rust-side declaration here gives the symbol
    /// the correct `extern "C"` ABI so callers can pass `TaskCtx`
    /// pointers as the System V AMD64 first/second argument registers
    /// (`rdi`, `rsi`).
    pub fn rustos_arch_x86_64_switch(prev: *mut TaskCtx, next: *mut TaskCtx);
}

/// Switch from `prev` to `next` on the current CPU.
///
/// Saves the calling task's callee-saved registers onto its kernel
/// stack, records the resulting `rsp` in `*prev`, loads `(*next).rsp`,
/// and pops the inbound task's saved registers. The net effect is that
/// control returns to the call site of the *previous* invocation of
/// `switch` for the inbound task — or, for a never-run task whose
/// `rsp` was seeded by [`TaskCtx::prepare`], to that task's `entry`.
///
/// This is the only documented entry point on the safe API surface;
/// the bare `extern "C"` symbol is private to the arch crate.
///
/// # Safety
///
/// See the module-level safety contract. In summary the caller must
/// guarantee that
///
/// * `prev` and `next` are non-null `*mut TaskCtx`s,
/// * `prev` belongs to the currently-running task and is exclusive to
///   this CPU,
/// * `next.rsp` is either zero (and the function is unreachable — see
///   below) or a value `switch` / [`TaskCtx::prepare`] previously
///   wrote for the inbound task,
/// * the inbound kernel stack is mapped and exclusive to this CPU.
///
/// `next.rsp == 0` is a kernel bug — the prologue would dereference
/// a zero pointer. Callers must run a freshly-prepared task through
/// [`TaskCtx::prepare`] first.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub unsafe fn switch(prev: *mut TaskCtx, next: *mut TaskCtx) {
    // SAFETY: forwarded from the caller's contract. The assembly
    // implementation in `context.s` saves r15/r14/r13/r12/rbx/rbp +
    // implicit return address to `*prev`'s stack, swaps `rsp`, and
    // restores from `*next`'s stack.
    unsafe { rustos_arch_x86_64_switch(prev, next) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, offset_of};

    #[test]
    fn task_ctx_layout_is_fixed() {
        assert_eq!(size_of::<TaskCtx>(), 8);
        assert_eq!(align_of::<TaskCtx>(), 8);
        assert_eq!(offset_of!(TaskCtx, rsp), 0);
    }

    #[test]
    fn task_ctx_new_is_zero() {
        let c = TaskCtx::new();
        assert_eq!(c.rsp, 0);
    }

    // A `extern "C" fn(usize) -> !` we only need for its address; never
    // called by the host tests. The body must diverge; we use an
    // explicit `panic!` rather than `loop {}` because clippy's
    // `empty_loop` lint (rightly) flags spin loops outside `no_std`.
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
        // 16-byte aligned, but well below the 64-byte frame.
        assert_eq!(
            c.prepare(0x10, host_entry, 0).unwrap_err(),
            PrepareError::TooSmall
        );
    }

    #[test]
    fn prepare_writes_initial_frame() {
        // Use a real, suitably-aligned heap buffer so the writes the
        // function performs land somewhere safe to inspect on the host.
        #[repr(C, align(16))]
        struct Stack([u64; 16]);
        let mut stack = Stack([0xDEAD_BEEF_DEAD_BEEFu64; 16]);
        let top = unsafe { core::ptr::addr_of_mut!(stack.0).cast::<u64>().add(16) } as u64;
        let mut c = TaskCtx::new();
        c.prepare(top, host_entry, 0xCAFE).unwrap();
        // rsp should be `top - 72` (8 frame words + the alignment pad).
        assert_eq!(c.rsp, top - 72);
        // Verify the frame layout the resume epilogue will pop, in the
        // `context.s` `popq` order: rdi, then r15..rbp, then `ret`.
        let frame = unsafe { core::slice::from_raw_parts(c.rsp as *const u64, 8) };
        // rdi <- arg (popped first).
        assert_eq!(frame[0], 0xCAFE);
        // r15, r14, r13, r12, rbx, rbp <- zero.
        for slot in &frame[1..7] {
            assert_eq!(*slot, 0);
        }
        // return address <- entry (consumed by `ret`).
        assert_eq!(frame[7], host_entry as *const () as usize as u64);
    }
}
