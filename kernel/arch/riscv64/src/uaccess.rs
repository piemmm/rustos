//! riscv64 fault-windowed user-copy routine — the port's backing for
//! the Arch HAL guarded-copy slot (`rustos_arch_api::uaccess`).
//!
//! The kernel's validated user-copy path moves bytes through the direct
//! physical map only after proving every page mapped and permissioned,
//! so a mid-copy fault indicates that proof was violated underneath it
//! (a kernel defect). This module turns that fault into an error return
//! instead of the port's fatal path: every load and store of the copy
//! sits inside the exported **fault window**
//! `[rustos_riscv64_uaccess_window_begin, rustos_riscv64_uaccess_window_end)`,
//! and the S-mode trap handler ([`crate::trap`]), on a load/store page
//! fault taken from S-mode whose saved `sepc` lies inside that window,
//! rewrites the frame's `sepc` to `rustos_riscv64_uaccess_fixup` — the
//! epilogue's `sret` then resumes at the fix-up, which returns `1`
//! ("faulted") to the caller. No register beyond the `a0` return needs
//! repair: the fix-up itself sets it, and the routine keeps its whole
//! state in caller-saved registers the trap frame already preserves.
//!
//! # Copy shape
//!
//! Byte moves until the destination is 8-byte aligned, then a
//! doubleword loop while source and destination are mutually aligned,
//! then a byte tail — the standard `memcpy` shape, chosen because a
//! misaligned `ld`/`sd` may trap on riscv64 silicon (the fault window
//! must only ever absorb *page* faults, never a self-inflicted
//! misaligned-access trap).
//!
//! The routine is installed into the Arch HAL slot by
//! `crate::trap::install_trap_vector`, the one chokepoint every
//! consumer of the S-mode vector already runs through, so the recovery
//! is armed before any syscall copy can execute.

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
core::arch::global_asm!(
    r#"
.section .text
.balign 4
.global rustos_riscv64_guarded_user_copy
.global rustos_riscv64_uaccess_window_begin
.global rustos_riscv64_uaccess_window_end
.global rustos_riscv64_uaccess_fixup
# usize rustos_riscv64_guarded_user_copy(u8 *dst, const u8 *src, usize len)
# Returns 0 on success; 1 when a page fault inside the window was
# redirected to the fix-up. State lives in t0/t1/t2 (caller-saved, so a
# nested interrupt's trap frame preserves them).
rustos_riscv64_guarded_user_copy:
    mv      t0, a0
    mv      t1, a1
    mv      t2, a2
rustos_riscv64_uaccess_window_begin:
1:  # head: byte-copy until dst is 8-aligned (or the range is done)
    beqz    t2, 5f
    andi    t3, t0, 7
    beqz    t3, 2f
    lbu     t4, 0(t1)
    sb      t4, 0(t0)
    addi    t0, t0, 1
    addi    t1, t1, 1
    addi    t2, t2, -1
    j       1b
2:  # dst 8-aligned; only a mutually aligned src takes the word loop
    andi    t3, t1, 7
    bnez    t3, 4f
3:  # doubleword loop while at least 8 bytes remain
    li      t3, 8
    bltu    t2, t3, 4f
    ld      t4, 0(t1)
    sd      t4, 0(t0)
    addi    t0, t0, 8
    addi    t1, t1, 8
    addi    t2, t2, -8
    j       3b
4:  # byte tail (and the mutually-misaligned body)
    beqz    t2, 5f
    lbu     t4, 0(t1)
    sb      t4, 0(t0)
    addi    t0, t0, 1
    addi    t1, t1, 1
    addi    t2, t2, -1
    j       4b
5:
rustos_riscv64_uaccess_window_end:
    li      a0, 0
    ret
.balign 4
rustos_riscv64_uaccess_fixup:
    li      a0, 1
    ret
"#
);

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
extern "C" {
    /// The fault-windowed copy routine published by the asm block above.
    /// Matches [`rustos_arch_api::uaccess::GuardedCopyFn`].
    fn rustos_riscv64_guarded_user_copy(dst: *mut u8, src: *const u8, len: usize) -> usize;
    /// First instruction of the fault window (inclusive).
    fn rustos_riscv64_uaccess_window_begin();
    /// First instruction past the fault window (exclusive).
    fn rustos_riscv64_uaccess_window_end();
    /// Recovery entry: returns `1` from the interrupted copy.
    fn rustos_riscv64_uaccess_fixup();
}

/// Publish the routine into the Arch HAL guarded-copy slot.
///
/// Idempotent (the slot accepts a re-install of the same routine), so
/// every caller of [`crate::trap::install_trap_vector`] may run it
/// unconditionally.
///
/// # Errors
///
/// [`rustos_arch_api::uaccess::InstallGuardedCopyError`] when a
/// *different* routine already occupies the slot — a boot-order defect
/// the caller must treat as fatal (fail closed).
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub fn install() -> Result<(), rustos_arch_api::uaccess::InstallGuardedCopyError> {
    rustos_arch_api::uaccess::install_guarded_copy(
        rustos_riscv64_guarded_user_copy
            as unsafe extern "C" fn(*mut u8, *const u8, usize) -> usize,
    )
}

/// If `pc` (a saved `sepc` from a kernel-mode data page fault) lies
/// inside the copy's fault window, return the fix-up address the trap
/// handler must rewrite the frame's `sepc` to; `None` for every PC
/// outside the window (the fault is not ours and stays on the fatal
/// path).
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
#[must_use]
pub fn kernel_fixup_for(pc: u64) -> Option<u64> {
    let begin = rustos_riscv64_uaccess_window_begin as *const () as usize as u64;
    let end = rustos_riscv64_uaccess_window_end as *const () as usize as u64;
    if rustos_arch_api::uaccess::pc_in_window(pc, begin, end) {
        Some(rustos_riscv64_uaccess_fixup as *const () as usize as u64)
    } else {
        None
    }
}
