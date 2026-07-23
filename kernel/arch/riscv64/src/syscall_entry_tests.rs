//! Host unit tests for the riscv64 `ecall` syscall entry path.

use super::*;
use core::mem::offset_of;
use std::sync::atomic::{AtomicU64, Ordering};

#[test]
fn ecall_cause_codes_match_privileged_spec() {
    assert_eq!(SCAUSE_ECALL_FROM_U, 8);
    assert_eq!(SCAUSE_ECALL_FROM_S, 9);
    assert_eq!(ECALL_INSTR_LEN, 4);
}

#[test]
fn is_ecall_from_user_matches_only_the_u_mode_synchronous_cause() {
    assert!(is_ecall_from_user(SCAUSE_ECALL_FROM_U));
    // S-mode ecall is a different cause.
    assert!(!is_ecall_from_user(SCAUSE_ECALL_FROM_S));
    // The same code with the interrupt bit set is a (nonsensical) IRQ,
    // never a syscall.
    assert!(!is_ecall_from_user(
        SCAUSE_INTERRUPT_BIT | SCAUSE_ECALL_FROM_U
    ));
}

#[test]
fn pack_raw_args_orders_a0_through_a5() {
    let a = pack_raw_args(0x10, 0x20, 0x30, 0x40, 0x50, 0x60);
    assert_eq!(a, [0x10, 0x20, 0x30, 0x40, 0x50, 0x60]);
    assert_eq!(a.len(), SYSCALL_MAX_ARGS);
}

#[test]
fn trap_frame_layout_matches_trap_s_offsets() {
    // The asm in `trap.s` stores these registers at these byte offsets.
    assert_eq!(offset_of!(TrapFrame, ra), 0);
    assert_eq!(offset_of!(TrapFrame, t0), 8);
    assert_eq!(offset_of!(TrapFrame, t6), 56);
    assert_eq!(offset_of!(TrapFrame, a0), 64);
    assert_eq!(offset_of!(TrapFrame, a5), 104);
    assert_eq!(offset_of!(TrapFrame, a7), 120);
    // The callee-saved set (s0=fp .. s11) the vector saves for the
    // user-fault crash backtrace, appended after the caller-saved GPRs.
    assert_eq!(offset_of!(TrapFrame, s0), 128);
    assert_eq!(offset_of!(TrapFrame, s11), 216);
    // The return-state CSRs the redesigned vector saves, appended after
    // the GP registers (their offsets are pinned by the `OFF_*` `.equ`s
    // in `trap.s`).
    assert_eq!(offset_of!(TrapFrame, sepc), 224);
    assert_eq!(offset_of!(TrapFrame, sstatus), 232);
    assert_eq!(offset_of!(TrapFrame, user_sp), 240);
    // The struct packs 31 u64 fields (248 bytes, offset 240 + 8); the asm
    // reserves TRAP_FRAME_SIZE = 256 so the kernel stack stays 16-byte
    // aligned, the top 8 bytes being alignment padding.
    assert_eq!(core::mem::size_of::<TrapFrame>(), 248);
}

/// Records the (number, args) it was handed and returns a fixed value.
static SEEN_NUMBER: AtomicU64 = AtomicU64::new(0);
static SEEN_ARG0: AtomicU64 = AtomicU64::new(0);

extern "C" fn recording_cb(number: u64, args_ptr: *const [u64; SYSCALL_MAX_ARGS]) -> u64 {
    // SAFETY: `dispatch_ecall` passes a pointer to a live stack array.
    let args = unsafe { &*args_ptr };
    SEEN_NUMBER.store(number, Ordering::SeqCst);
    SEEN_ARG0.store(args[0], Ordering::SeqCst);
    0xABCD
}

#[test]
fn dispatch_ecall_fails_closed_without_a_callback() {
    clear_dispatch_for_tests();
    let mut frame = TrapFrame {
        a7: 7,
        ..TrapFrame::default()
    };
    assert!(!dispatch_ecall(&mut frame));
    // Frame untouched on the fail-closed path.
    assert_eq!(frame.a0, 0);
}

#[test]
fn dispatch_ecall_forwards_number_and_args_and_writes_return() {
    clear_dispatch_for_tests();
    set_dispatch_callback(recording_cb);
    let mut frame = TrapFrame {
        a7: 42,     // syscall number
        a0: 0x1111, // first argument
        a1: 0x2222,
        ..TrapFrame::default()
    };
    assert!(dispatch_ecall(&mut frame));
    assert_eq!(SEEN_NUMBER.load(Ordering::SeqCst), 42);
    assert_eq!(SEEN_ARG0.load(Ordering::SeqCst), 0x1111);
    // Return value landed in a0.
    assert_eq!(frame.a0, 0xABCD);
    clear_dispatch_for_tests();
}

#[test]
fn dispatch_callback_round_trips() {
    clear_dispatch_for_tests();
    assert!(dispatch_callback().is_none());
    set_dispatch_callback(recording_cb);
    assert_eq!(
        dispatch_callback().map(|f| f as *const () as usize),
        Some(recording_cb as *const () as usize)
    );
    clear_dispatch_for_tests();
}
