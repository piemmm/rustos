//! Host unit tests for [`RiscvArch`] (`AGENTS.md` §7 — tests live in
//! their own file beside the code they cover).

use super::*;

#[test]
fn current_cpu_returns_boot_cpu() {
    let arch = RiscvArch::new(0, 10_000_000);
    assert_eq!(arch.current_cpu(), 0);
}

#[test]
fn ticks_now_is_monotonic_on_host() {
    let arch = RiscvArch::new(0, 10_000_000);
    let a = arch.ticks_now();
    let b = arch.ticks_now();
    assert!(b > a);
}

#[test]
fn monotonic_ns_is_non_decreasing_on_host() {
    // The host clock counts ticks 1, 2, 3, …; at 1 GHz a tick is one
    // nanosecond, so the readings are strictly increasing.
    let arch = RiscvArch::new(0, 1_000_000_000);
    let a = arch.monotonic_ns();
    let b = arch.monotonic_ns();
    let c = arch.monotonic_ns();
    assert!(b >= a, "expected b >= a, got a={a} b={b}");
    assert!(c >= b, "expected c >= b, got b={b} c={c}");
}

#[test]
fn timebase_is_round_tripped() {
    let arch = RiscvArch::new(3, 24_000_000);
    assert_eq!(arch.timebase_hz(), 24_000_000);
}

#[test]
fn zero_timebase_does_not_divide_by_zero() {
    // A malformed (zero) frequency must not trap; `monotonic_ns` clamps
    // the divisor to 1 (`AGENTS.md` §2.9 — fail safe).
    let arch = RiscvArch::new(0, 0);
    let _ = arch.monotonic_ns();
}
