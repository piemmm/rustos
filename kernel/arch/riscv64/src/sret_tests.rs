//! Host unit tests for the ordering of this port's two `sret` sequences.
//!
//! `sscratch` is the trap vector's *only* discriminator between a trap from
//! U-mode and one from S-mode: a non-zero swap-in result means "from U". An
//! S-mode interrupt taken while `sscratch` is armed is therefore misread as
//! a U-mode trap — it builds its frame at the armed stack top, clobbering
//! the interrupted frame, and returns down the S path, which does not
//! re-arm. `sscratch` is then 0 while a task runs in U-mode, so every later
//! trap of that task builds the kernel's frame on the task's own *user*
//! stack: silent corruption, ending in a wild jump that once parked the
//! hart. Both sequences therefore mask S-mode interrupts *before* they arm
//! `sscratch`.
//!
//! Neither sequence can be executed on the host, and the window is a race
//! no target test can reliably enter, so the ordering is pinned here
//! against the two sources — the assembly carve-out `trap.s` and the
//! inline-`asm!` user entry. The needles live in this file rather than in
//! the inspected ones, so a needle can never match itself and pass a test
//! whose subject has lost its mask.

/// Byte offset of the sole occurrence of `needle` in `src`.
///
/// Requiring exactly one occurrence is what makes the ordering assertions
/// meaningful: a second copy of an arming or masking instruction would
/// leave the order they are compared in ambiguous.
fn only(src: &str, needle: &str) -> usize {
    let mut hits = src.match_indices(needle);
    let Some((at, _)) = hits.next() else {
        panic!("no `{needle}` in the inspected source");
    };
    assert!(
        hits.next().is_none(),
        "`{needle}` must appear exactly once in the inspected source",
    );
    at
}

#[test]
fn the_trap_epilogue_restores_sstatus_before_it_re_arms_sscratch() {
    let src = include_str!("trap.s");
    let handler = only(src, "call    tairix_riscv64_trap_handler");
    let sstatus = only(src, "csrw    sstatus, t0");
    let arm = only(src, "csrw    sscratch, t1");
    let u_return = src[arm..].find("sret").expect("the U-mode sret") + arm;

    assert!(
        handler < sstatus,
        "the restore belongs to the return path, after the handler call",
    );
    assert!(
        sstatus < arm,
        "`sscratch` is re-armed before the saved `sstatus` re-masks S-mode \
         interrupts, so an interrupt in the window is misread as from U-mode",
    );
    assert!(
        arm < u_return,
        "the arm precedes the `sret` that consumes it"
    );
}

/// The handler runs with `sscratch` forced to 0 so a nested S-mode trap —
/// including one taken because a syscall body deliberately enabled `SIE` —
/// is classified correctly.
#[test]
fn the_trap_entry_clears_sscratch_for_the_handler() {
    let src = include_str!("trap.s");
    let clear = only(src, "csrw    sscratch, zero");
    let handler = only(src, "call    tairix_riscv64_trap_handler");

    assert!(
        clear < handler,
        "the handler must run with `sscratch` already 0",
    );
}

#[test]
fn the_user_entry_masks_supervisor_interrupts_before_it_arms_sscratch() {
    let src = include_str!("userentry.rs");
    let mask = only(src, "\"csrc sstatus, {clr}\",");
    let arm = only(src, "\"csrw sscratch, sp\",");
    let sret = only(src, "\"sret\",");

    assert!(
        mask < arm,
        "`sscratch` is armed with S-mode interrupts still enabled, so an \
         interrupt in the window is misread as a trap from U-mode",
    );
    assert!(arm < sret, "the arm precedes the `sret` that consumes it");
}

/// Nothing between the mask and the `sret` may re-enable S-mode interrupts,
/// so the masked window reaches the transition intact.
#[test]
fn the_user_entry_masked_window_reaches_the_sret_intact() {
    let src = include_str!("userentry.rs");
    let mask = only(src, "\"csrc sstatus, {clr}\",");
    let sret = only(src, "\"sret\",");

    for line in src[mask..sret].lines().skip(1) {
        assert!(
            !line.contains("csrs sstatus"),
            "the masked window must reach the `sret` intact, but it runs `{}`",
            line.trim(),
        );
    }
}
