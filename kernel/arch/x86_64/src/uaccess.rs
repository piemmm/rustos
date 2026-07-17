//! x86_64 fault-windowed user-copy routine — the port's backing for
//! the Arch HAL guarded-copy slot (`tairix_arch_api::uaccess`).
//!
//! The kernel's validated user-copy path moves bytes through the direct
//! physical map only after proving every page mapped and permissioned,
//! so a mid-copy fault indicates that proof was violated underneath it
//! (a kernel defect). This module turns that fault into an error return
//! instead of the port's fatal path: the copy is a single `rep movsb`
//! inside the exported **fault window**
//! `[tairix_x86_64_uaccess_window_begin, tairix_x86_64_uaccess_window_end)`,
//! and the dedicated `#PF` entry ([`crate::fault`]), on a kernel-mode
//! page fault whose pushed `RIP` lies inside that window, rewrites the
//! interrupt frame's `RIP` to `tairix_x86_64_uaccess_fixup` — the
//! stub's `iretq` then resumes at the fix-up, which returns `1`
//! ("faulted") to the caller. No register beyond the `rax` return needs
//! repair: the fix-up itself sets it, and the stub restores every GP
//! register from its saves.
//!
//! # Copy shape
//!
//! `rep movsb` — the idiomatic bulk copy on modern x86_64 (ERMSB moves
//! cache-line-sized chunks internally), and the ideal fault-window body:
//! one restartable instruction, with `rcx`/`rsi`/`rdi` architecturally
//! consistent at the fault boundary.
//!
//! Unlike the riscv64/aarch64 ports, whose trap-vector installers arm
//! the slot, this port has no single vector-init function: the boot
//! path pairs `install` with its dedicated-`#PF`-entry install (see
//! `tairix-kernel`'s x86_64 boot), and so must any test binary that
//! exercises the window.

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
core::arch::global_asm!(
    r#"
.section .text
.balign 16
.global tairix_x86_64_guarded_user_copy
.global tairix_x86_64_uaccess_window_begin
.global tairix_x86_64_uaccess_window_end
.global tairix_x86_64_uaccess_fixup
# usize tairix_x86_64_guarded_user_copy(u8 *dst /*rdi*/, const u8 *src /*rsi*/, usize len /*rdx*/)
# Returns 0 in rax on success; 1 when a page fault inside the window was
# redirected to the fix-up.
tairix_x86_64_guarded_user_copy:
    movq    %rdx, %rcx
    xorl    %eax, %eax
tairix_x86_64_uaccess_window_begin:
    rep movsb
tairix_x86_64_uaccess_window_end:
    retq
.balign 16
tairix_x86_64_uaccess_fixup:
    movl    $1, %eax
    retq
"#,
    options(att_syntax)
);

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
extern "C" {
    /// The fault-windowed copy routine published by the asm block above.
    /// Matches [`tairix_arch_api::uaccess::GuardedCopyFn`].
    fn tairix_x86_64_guarded_user_copy(dst: *mut u8, src: *const u8, len: usize) -> usize;
    /// First instruction of the fault window (inclusive).
    fn tairix_x86_64_uaccess_window_begin();
    /// First instruction past the fault window (exclusive).
    fn tairix_x86_64_uaccess_window_end();
    /// Recovery entry: returns `1` from the interrupted copy.
    fn tairix_x86_64_uaccess_fixup();
}

/// Publish the routine into the Arch HAL guarded-copy slot.
///
/// Idempotent (the slot accepts a re-install of the same routine).
/// Paired with the dedicated `#PF` entry install on the boot path: the
/// entry's kernel-fault window check is what redirects an in-window
/// fault to the fix-up, so the two arm together.
///
/// # Errors
///
/// [`tairix_arch_api::uaccess::InstallGuardedCopyError`] when a
/// *different* routine already occupies the slot — a boot-order defect
/// the caller must treat as fatal (fail closed).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn install() -> Result<(), tairix_arch_api::uaccess::InstallGuardedCopyError> {
    tairix_arch_api::uaccess::install_guarded_copy(
        tairix_x86_64_guarded_user_copy as unsafe extern "C" fn(*mut u8, *const u8, usize) -> usize,
    )
}

/// If `pc` (the pushed `RIP` of a kernel-mode `#PF`) lies inside the
/// copy's fault window, return the fix-up address the `#PF` dispatcher
/// must rewrite the interrupt frame's `RIP` slot to; `None` for every
/// PC outside the window (the fault is not ours and stays on the fatal
/// path).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[must_use]
pub fn kernel_fixup_for(pc: u64) -> Option<u64> {
    let begin = tairix_x86_64_uaccess_window_begin as *const () as usize as u64;
    let end = tairix_x86_64_uaccess_window_end as *const () as usize as u64;
    if tairix_arch_api::uaccess::pc_in_window(pc, begin, end) {
        Some(tairix_x86_64_uaccess_fixup as *const () as usize as u64)
    } else {
        None
    }
}
