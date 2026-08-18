//! Host unit tests for the atomicity of this port's two `eret` sequences.
//!
//! `ELR_EL1` and `SPSR_EL1` are single-copy registers holding the state an
//! `eret` consumes. An exception taken once they hold that state overwrites
//! both in hardware, and the nested handler's own return restores *its*
//! saved pair — so the interrupted sequence then `eret`s to the nested
//! handler's return address in the nested handler's PSTATE. In the trap
//! trampoline that resumes the epilogue at EL1 with the frame already
//! popped, walking `sp` one frame per turn off the kernel stack until the
//! loads fault, and then faulting recursively with `DAIF` masked: a silent,
//! unrecoverable wedge reported as a bare hard lockup. The debug watchdog's
//! Group-0/FIQ cadence is a live source of exactly that exception, because
//! the syscall/fault handler runs with `DAIF.F` clear so a wedged core can
//! be sampled (`plans/WATCHDOG.md`).
//!
//! Both sequences therefore mask every asynchronous exception before they
//! program the return state. Neither can be executed on the host, and the
//! window is a race no target test can reliably enter, so the ordering is
//! pinned here against the two sources — the assembly carve-out
//! `vectors.s` and the inline-`asm!` user entry. The needles live in this
//! file rather than in the inspected ones, so a needle can never match
//! itself and pass a test whose subject has lost its mask.

use std::string::String;
use std::vec::Vec;

/// Collapse each line's internal whitespace to single spaces so an
/// assertion names an instruction without also pinning the source's column
/// alignment.
fn instruction_lines(src: &str) -> Vec<String> {
    src.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

/// The index of the single line equal to `instruction`.
///
/// Requiring exactly one occurrence is what makes the ordering assertions
/// below meaningful: a second copy of a return-state write elsewhere in the
/// file would leave the order they are compared in ambiguous.
fn line_of(lines: &[String], instruction: &str) -> usize {
    let mut hits = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.as_str() == instruction);
    let Some((index, _)) = hits.next() else {
        panic!("no `{instruction}` line in the inspected source");
    };
    assert!(
        hits.next().is_none(),
        "`{instruction}` must appear exactly once in the inspected source",
    );
    index
}

/// Panic if any instruction in `lines` re-enables an asynchronous
/// exception, so the masked window reaches the `eret` intact.
fn assert_nothing_unmasks(lines: &[String]) {
    for line in lines {
        if line.starts_with("//") {
            continue;
        }
        let lowered = line.to_ascii_lowercase();
        assert!(
            !lowered.contains("daifclr") && !lowered.starts_with("msr daif,"),
            "the masked window must reach the `eret` intact, but it runs `{line}`",
        );
    }
}

#[test]
fn the_trap_epilogue_masks_asynchronous_exceptions_before_the_return_state() {
    let lines = instruction_lines(include_str!("vectors.s"));
    let handler = line_of(&lines, "bl tairix_aarch64_trap_handler");
    let mask = line_of(&lines, "msr DAIFSet, #0xf");
    let elr = line_of(&lines, "msr ELR_EL1, x2");
    let spsr = line_of(&lines, "msr SPSR_EL1, x3");
    let eret = line_of(&lines, "eret");

    assert!(
        handler < mask,
        "the mask belongs to the return path, after the handler call",
    );
    assert!(
        mask < elr && mask < spsr,
        "the return state must be programmed with exceptions already masked",
    );
    assert!(
        elr < eret && spsr < eret,
        "the return state is programmed before the `eret` consumes it",
    );
    assert_nothing_unmasks(&lines[mask..eret]);
}

#[test]
fn the_user_entry_eret_masks_asynchronous_exceptions_before_the_return_state() {
    let lines = instruction_lines(include_str!("userentry.rs"));
    let mask = line_of(&lines, "\"msr DAIFSet, #0xf\",");
    let elr = line_of(&lines, "\"msr ELR_EL1, {entry}\",");
    let spsr = line_of(&lines, "\"msr SPSR_EL1, {spsr}\",");
    let eret = line_of(&lines, "\"eret\",");

    assert!(
        mask < elr && mask < spsr,
        "the EL0 entry state must be programmed with exceptions already masked",
    );
    assert!(
        elr < eret && spsr < eret,
        "the entry state is programmed before the `eret` consumes it",
    );
    assert_nothing_unmasks(&lines[mask..eret]);
}

/// The psABI thread pointer must be saved on entry and restored on the way
/// out, at the same frame offset, so several threads of one process do not
/// share one thread-local storage base (`plans/THREADS.md` decision 7).
#[test]
fn the_trap_frame_carries_the_thread_pointer_across_a_context_switch() {
    let lines = instruction_lines(include_str!("vectors.s"));
    let handler = line_of(&lines, "bl tairix_aarch64_trap_handler");
    let save = line_of(&lines, "mrs x2, TPIDR_EL0");
    let store = line_of(&lines, "str x2, [sp, #800]");
    let load = line_of(&lines, "ldr x2, [sp, #800]");
    let restore = line_of(&lines, "msr TPIDR_EL0, x2");
    let eret = line_of(&lines, "eret");

    assert!(
        save < handler && store < handler,
        "the thread pointer belongs to the entry path, before the handler call",
    );
    assert_eq!(save + 1, store, "the read is stored straight away");
    assert!(
        handler < load,
        "the reload belongs to the return path, after the handler call",
    );
    assert_eq!(
        load + 1,
        restore,
        "the loaded word is written straight back"
    );
    assert!(
        restore < eret,
        "the thread pointer is restored before the `eret` resumes the thread",
    );
}

/// A freshly entered thread gets its own thread pointer rather than whatever
/// the previous occupant of the CPU left in the register.
#[test]
fn the_user_entry_seeds_the_thread_pointer() {
    let lines = instruction_lines(include_str!("userentry.rs"));
    let mask = line_of(&lines, "\"msr DAIFSet, #0xf\",");
    let tls = line_of(&lines, "\"msr TPIDR_EL0, {tls}\",");
    let eret = line_of(&lines, "\"eret\",");

    assert!(mask < tls, "programmed with exceptions already masked");
    assert!(tls < eret, "seeded before the `eret` consumes it");
}
