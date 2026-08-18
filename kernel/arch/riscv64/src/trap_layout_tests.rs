//! Host unit tests pinning the riscv64 trap protocol against `trap.s`.
//!
//! Two things must agree with the assembly carve-out and cannot be checked by
//! running it (the vector executes only on the freestanding target):
//!
//! 1. **Layout.** Every `.equ` in `trap.s` is parsed out of the source and
//!    compared against the [`TrapFrame`] field it addresses (or the Rust
//!    constant that mirrors it). The offsets therefore have exactly one
//!    definition — the assembly — instead of being hand-copied into a test.
//! 2. **The `tp` discipline.** `tp` is simultaneously the RISC-V psABI thread
//!    pointer that U-mode writes freely and this port's per-hart kernel
//!    identity anchor ([`crate::smp::current_hartid`]). A vector that let a
//!    U-mode-supplied `tp` survive into the handler would let a task steer the
//!    kernel onto another hart's per-CPU state. The from-U prologue must
//!    therefore spill the user's value and reload the kernel's from the task's
//!    trap anchor before anything else reads `tp`.
//!
//! The needles live here rather than in `trap.s`, so a needle can never match
//! itself and pass a test whose subject has lost the instruction.

use super::{TrapFrame, TRAP_ANCHOR_BYTES, TRAP_ANCHOR_KTP_OFFSET, TRAP_FRAME_BYTES};
use core::mem::{offset_of, size_of};
use std::format;
use std::string::String;
use std::vec::Vec;

/// The `trap.s` source the assertions below inspect.
const TRAP_S: &str = include_str!("trap.s");

/// Value of `.equ <name>, <literal>` in `trap.s`.
///
/// Only integer-literal `.equ`s are resolved: an expression-valued one (such
/// as `OFF_UTP_PRE`) is checked by [`equ_expr`] instead, so a malformed or
/// renamed definition fails the test rather than silently reading as zero.
fn equ(name: &str) -> u64 {
    let prefix = format!(".equ {name},");
    let line = TRAP_S
        .lines()
        .find(|l| l.trim_start().starts_with(&prefix))
        .unwrap_or_else(|| panic!("no `.equ {name}, …` in trap.s"));
    let value = line
        .split(',')
        .nth(1)
        .unwrap_or_else(|| panic!("`.equ {name}` has no value"))
        .trim();
    value
        .parse()
        .unwrap_or_else(|e| panic!("`.equ {name}, {value}` is not an integer: {e}"))
}

/// Raw right-hand side of `.equ <name>, <expr>` in `trap.s`, whitespace
/// normalised, for the definitions whose value is an expression over other
/// `.equ`s rather than a literal.
fn equ_expr(name: &str) -> String {
    let prefix = format!(".equ {name},");
    let line = TRAP_S
        .lines()
        .find(|l| l.trim_start().starts_with(&prefix))
        .unwrap_or_else(|| panic!("no `.equ {name}, …` in trap.s"));
    line.split_once(',')
        .expect("checked by the prefix match")
        .1
        .split('#')
        .next()
        .expect("split always yields one element")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Byte offset of the sole occurrence of `needle` in `TRAP_S`.
///
/// Requiring exactly one occurrence is what makes the ordering assertions
/// meaningful: a second copy of a spill or reload would leave the order they
/// are compared in ambiguous.
fn only(needle: &str) -> usize {
    let mut hits = TRAP_S.match_indices(needle);
    let Some((at, _)) = hits.next() else {
        panic!("no `{needle}` in trap.s");
    };
    assert!(
        hits.next().is_none(),
        "`{needle}` must appear exactly once in trap.s",
    );
    at
}

#[test]
fn the_frame_size_equ_matches_the_rust_frame() {
    assert_eq!(equ("TRAP_FRAME_SIZE"), TRAP_FRAME_BYTES);
    assert_eq!(TRAP_FRAME_BYTES, size_of::<TrapFrame>() as u64);
}

#[test]
fn every_csr_slot_equ_matches_its_frame_field() {
    assert_eq!(equ("OFF_SEPC"), offset_of!(TrapFrame, sepc) as u64);
    assert_eq!(equ("OFF_SSTATUS"), offset_of!(TrapFrame, sstatus) as u64);
    assert_eq!(equ("OFF_USP"), offset_of!(TrapFrame, user_sp) as u64);
    assert_eq!(equ("OFF_UTP"), offset_of!(TrapFrame, user_tp) as u64);
}

/// The alignment and containment relations between these constants are
/// compile-time `const _: () = assert!(…)`s beside their definitions; what a
/// test must still check is that the assembly agrees with them.
#[test]
fn the_anchor_equs_match_their_rust_constants() {
    assert_eq!(equ("TRAP_ANCHOR_BYTES"), TRAP_ANCHOR_BYTES);
    assert_eq!(equ("OFF_ANCHOR_KTP"), TRAP_ANCHOR_KTP_OFFSET);
}

/// Both prologues spill the interrupted `tp` *before* `sp` moves down to the
/// frame base, so the offset they use must be the frame slot biased by the
/// frame size.
#[test]
fn the_pre_adjustment_tp_offset_is_the_frame_slot_biased_by_the_frame_size() {
    assert_eq!(equ_expr("OFF_UTP_PRE"), "OFF_UTP - TRAP_FRAME_SIZE");
}

/// The security invariant: between the entry swap and the reload of the
/// kernel `tp`, the only instruction naming `tp` is the spill that saves the
/// user's value. If anything else read `tp` in that window it would be
/// reading a U-mode-supplied word.
#[test]
fn the_from_user_prologue_reloads_the_kernel_tp_before_the_handler_runs() {
    let swap = only("csrrw   sp, sscratch, sp");
    let reload = only("ld      tp, OFF_ANCHOR_KTP(sp)");
    let handler = only("call    tairix_riscv64_trap_handler");

    assert!(swap < reload, "the reload follows the entry swap");
    assert!(
        reload < handler,
        "the kernel `tp` must be restored before any Rust runs, or the \
         handler resolves its per-CPU state from a U-mode-supplied word",
    );

    // `sd tp, OFF_UTP_PRE(sp)` appears once per direction (from-U and the
    // nested-S path); every *other* mention of `tp` in the window is a defect.
    for (at, line) in TRAP_S[swap..reload].lines().enumerate() {
        let code = line.split('#').next().unwrap_or("");
        if !code.contains("tp") {
            continue;
        }
        assert!(
            code.contains("sd      tp, OFF_UTP_PRE(sp)"),
            "line {at} of the entry window touches `tp` other than to spill \
             it: `{}`",
            code.trim(),
        );
    }
}

/// The restore side: the epilogue reloads the interrupted `tp` from the
/// frame, and on the U-return path it publishes the *kernel* `tp` into the
/// anchor first — while that value is still live in the register.
#[test]
fn the_u_return_path_publishes_the_kernel_tp_before_it_restores_the_user_tp() {
    let publish = only("sd      tp, OFF_ANCHOR_KTP(t1)");
    let arm = only("csrw    sscratch, t1");
    let restore = only("ld      tp, OFF_UTP(sp)");
    let restore_macro = only(".macro RESTORE_GPRS");

    assert!(
        publish < arm,
        "the anchor must carry this hart's `tp` before `sscratch` names it",
    );
    // The reload lives inside `RESTORE_GPRS`, which both return paths invoke
    // *after* the publish above, so the ordering holds by construction.
    assert!(
        restore_macro < publish,
        "the macro definition precedes its use sites",
    );
    assert!(
        restore_macro < restore && restore < publish,
        "the `tp` reload belongs to RESTORE_GPRS, whose invocations follow \
         the publish",
    );
}

/// A nested S-mode trap keeps running on the kernel's own `tp`, so its frame
/// slot must still be written — otherwise the epilogue's unconditional reload
/// would restore an uninitialised word into the per-hart identity register.
#[test]
fn the_nested_supervisor_prologue_also_spills_tp() {
    let recover = only("csrr    sp, sscratch\n");
    let spills: Vec<usize> = TRAP_S
        .match_indices("sd      tp, OFF_UTP_PRE(sp)")
        .map(|(at, _)| at)
        .collect();
    assert_eq!(
        spills.len(),
        2,
        "one spill per trap direction (nested S-mode, and from U-mode)",
    );
    assert!(
        spills[0] > recover,
        "the nested-S spill follows its `sp` recovery",
    );
}
