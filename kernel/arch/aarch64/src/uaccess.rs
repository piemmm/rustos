//! aarch64 fault-windowed user-copy routine — the port's backing for
//! the Arch HAL guarded-copy slot (`rustos_arch_api::uaccess`).
//!
//! The kernel's validated user-copy path moves bytes through the direct
//! physical map only after proving every page mapped and permissioned,
//! so a mid-copy fault indicates that proof was violated underneath it
//! (a kernel defect). This module turns that fault into an error return
//! instead of the port's fatal path: every load and store of the copy
//! sits inside the exported **fault window**
//! `[rustos_aarch64_uaccess_window_begin, rustos_aarch64_uaccess_window_end)`,
//! and the EL1 trap handler ([`crate::exceptions`]), on a same-EL data
//! abort whose saved `ELR_EL1` lies inside that window, rewrites the
//! frame's ELR slot to `rustos_aarch64_uaccess_fixup` — the trampoline's
//! `eret` then resumes at the fix-up, which returns `1` ("faulted") to
//! the caller. No register beyond the `x0` return needs repair: the
//! fix-up itself sets it, and the trampoline restores every GP register
//! from the frame.
//!
//! # Copy shape
//!
//! A 16-byte `ldp`/`stp` loop with a byte tail. Unaligned accesses to
//! Normal memory are architecturally valid on this port
//! (`SCTLR_EL1.A == 0`), so no alignment head is needed; only a
//! translation/permission fault can interrupt the window, which is
//! exactly what it absorbs.
//!
//! The routine is installed into the Arch HAL slot by
//! `crate::exceptions::init_vectors`, the one chokepoint every
//! consumer of the EL1 vector table already runs through, so the
//! recovery is armed before any syscall copy can execute.

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
core::arch::global_asm!(
    r#"
.section .text
.balign 4
.global rustos_aarch64_guarded_user_copy
.global rustos_aarch64_uaccess_window_begin
.global rustos_aarch64_uaccess_window_end
.global rustos_aarch64_uaccess_fixup
// usize rustos_aarch64_guarded_user_copy(u8 *dst, const u8 *src, usize len)
// Returns 0 on success; 1 when a data abort inside the window was
// redirected to the fix-up. State lives in x9-x13 (corruptible temps the
// trap frame saves and restores like every other GP register).
rustos_aarch64_guarded_user_copy:
    mov     x9, x0
    mov     x10, x1
    mov     x11, x2
rustos_aarch64_uaccess_window_begin:
1:  // 16-byte pair loop
    cmp     x11, #16
    b.lo    2f
    ldp     x12, x13, [x10], #16
    stp     x12, x13, [x9], #16
    sub     x11, x11, #16
    b       1b
2:  // byte tail
    cbz     x11, 3f
    ldrb    w12, [x10], #1
    strb    w12, [x9], #1
    sub     x11, x11, #1
    b       2b
3:
rustos_aarch64_uaccess_window_end:
    mov     x0, #0
    ret
.balign 4
rustos_aarch64_uaccess_fixup:
    mov     x0, #1
    ret
"#
);

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
extern "C" {
    /// The fault-windowed copy routine published by the asm block above.
    /// Matches [`rustos_arch_api::uaccess::GuardedCopyFn`].
    fn rustos_aarch64_guarded_user_copy(dst: *mut u8, src: *const u8, len: usize) -> usize;
    /// First instruction of the fault window (inclusive).
    fn rustos_aarch64_uaccess_window_begin();
    /// First instruction past the fault window (exclusive).
    fn rustos_aarch64_uaccess_window_end();
    /// Recovery entry: returns `1` from the interrupted copy.
    fn rustos_aarch64_uaccess_fixup();
}

/// Publish the routine into the Arch HAL guarded-copy slot.
///
/// Idempotent (the slot accepts a re-install of the same routine), so
/// every caller of [`crate::exceptions::init_vectors`] may run it
/// unconditionally.
///
/// # Errors
///
/// [`rustos_arch_api::uaccess::InstallGuardedCopyError`] when a
/// *different* routine already occupies the slot — a boot-order defect
/// the caller must treat as fatal (fail closed).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn install() -> Result<(), rustos_arch_api::uaccess::InstallGuardedCopyError> {
    rustos_arch_api::uaccess::install_guarded_copy(
        rustos_aarch64_guarded_user_copy
            as unsafe extern "C" fn(*mut u8, *const u8, usize) -> usize,
    )
}

/// If `pc` (a saved `ELR_EL1` from a same-EL data abort) lies inside the
/// copy's fault window, return the fix-up address the trap handler must
/// rewrite the frame's ELR slot to; `None` for every PC outside the
/// window (the fault is not ours and stays on the fatal path).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[must_use]
pub fn kernel_fixup_for(pc: u64) -> Option<u64> {
    let begin = rustos_aarch64_uaccess_window_begin as *const () as usize as u64;
    let end = rustos_aarch64_uaccess_window_end as *const () as usize as u64;
    if rustos_arch_api::uaccess::pc_in_window(pc, begin, end) {
        Some(rustos_aarch64_uaccess_fixup as *const () as usize as u64)
    } else {
        None
    }
}
