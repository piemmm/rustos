extern crate std;

use std::string::String;

use super::{format_hex_word, KernelFault, Reporter, HEX_WORD_LEN, KERNEL_FAULT, KERNEL_PANIC};
use crate::backtrace::BootStackGuard;

const PORT: Reporter = Reporter {
    port: "aarch64",
    unit: "CPU",
};

/// The instruction abort a corrupted vtable slot raised in the
/// figure-determinism vertical: a branch to the `f64` bit pattern of `1.0`.
const VTABLE_BRANCH: KernelFault = KernelFault {
    syndrome: 0x8600_0000,
    address: 0x3ff0_0000_0000_0000,
    pc: 0x3ff0_0000_0000_0000,
};

fn fault_report(guard: Option<BootStackGuard>) -> String {
    let mut out = String::new();
    PORT.fault(
        &mut out,
        0,
        VTABLE_BRANCH,
        format_args!("ESR_EL1 0x86000000 (EC 0x21)"),
        guard,
    );
    out
}

fn last_line(report: &str) -> &str {
    report.lines().last().unwrap_or("")
}

#[test]
fn a_fault_report_ends_with_its_record_naming_the_syndrome() {
    let report = fault_report(Some(BootStackGuard::Intact));
    assert_eq!(
        last_line(&report),
        "[ERROR] id=4011 fatal kernel fault cpu=0 syndrome=0x0000000086000000 \
         fault_addr=0x3ff0000000000000 fault_pc=0x3ff0000000000000 boot_stack_guard=intact"
    );
    assert!(report.ends_with('\n'), "nothing trails the record");
}

#[test]
fn a_fault_report_says_why_in_prose_before_its_record() {
    let report = fault_report(None);
    let mut lines = report.lines().skip_while(|line| line.is_empty());
    assert_eq!(
        lines.next(),
        Some("==================== TAIRiX KERNEL FAULT ====================")
    );
    assert_eq!(
        lines.next(),
        Some(
            "[tairix-kernel] aarch64 exception on CPU 0 with no fault handler installed: \
             ESR_EL1 0x86000000 (EC 0x21)"
        )
    );
}

#[test]
fn a_report_with_no_guard_carries_no_guard_field() {
    let report = fault_report(None);
    assert!(!report.contains("boot-stack guard"));
    assert!(!last_line(&report).contains("boot_stack_guard"));
}

/// An overrun still in progress names both the verdict and its depth, in the
/// keys the kernel's own post-mortem uses.
#[test]
fn an_overrun_names_the_verdict_and_its_depth() {
    let report = fault_report(Some(BootStackGuard::BelowStack {
        sp: 0x4000_0f00,
        bytes: 0x100,
    }));
    assert!(last_line(&report)
        .ends_with("boot_stack_guard=sp_below_stack boot_stack_overrun_bytes=0x0000000000000100"));
    assert!(report.contains("boot-stack guard: OVERRUN - sp 0x40000f00"));
}

#[test]
fn a_disturbed_canary_is_named() {
    let report = fault_report(Some(BootStackGuard::Disturbed));
    assert!(last_line(&report).ends_with("boot_stack_guard=disturbed"));
}

#[test]
fn a_panic_report_ends_with_its_record_naming_where() {
    let here = core::panic::Location::caller();
    let mut report = String::new();
    PORT.panic(&mut report, 2, &"the grid did not place", Some(here), None);
    assert!(report.contains("[tairix-kernel] aarch64 panic on CPU 2: the grid did not place"));
    let expected = std::format!(
        "[ERROR] id=4010 kernel panic cpu=2 file={} line={} column={}",
        here.file(),
        here.line(),
        here.column()
    );
    assert_eq!(last_line(&report), expected);
}

#[test]
fn a_panic_with_no_location_says_so() {
    let mut report = String::new();
    Reporter {
        port: "riscv64",
        unit: "hart",
    }
    .panic(&mut report, 0, &"lost", None, None);
    assert!(report.contains("hart 0 halted"));
    assert_eq!(
        last_line(&report),
        "[ERROR] id=4010 kernel panic cpu=0 file=<unknown> line=0 column=0"
    );
}

#[test]
fn the_two_records_are_distinct() {
    assert_ne!(KERNEL_PANIC.id, KERNEL_FAULT.id);
    assert_ne!(KERNEL_PANIC.message, KERNEL_FAULT.message);
}

#[test]
fn a_fault_displays_as_the_records_fields() {
    assert_eq!(
        std::format!("{VTABLE_BRANCH}"),
        "syndrome=0x0000000086000000 fault_addr=0x3ff0000000000000 fault_pc=0x3ff0000000000000"
    );
}

#[test]
fn a_word_is_spelled_fixed_width_lowercase_hex() {
    for (value, spelled) in [
        (0, "0x0000000000000000"),
        (0xdead_beef, "0x00000000deadbeef"),
        (0xffff_8000_0000_1111, "0xffff800000001111"),
        (u64::MAX, "0xffffffffffffffff"),
    ] {
        let mut buf = [0u8; HEX_WORD_LEN];
        assert_eq!(format_hex_word(value, &mut buf), spelled);
    }
}
