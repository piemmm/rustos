//! Early-boot RAM self-test display and halt-on-fault wiring.
//!
//! The memory-test *engine* is architecture-neutral and lives in
//! `tairix_kernel_mem::ramtest`; this module is the kernel-core half that
//! drives it during the [`Phase::Mem`](crate::init) boot phase and shows the
//! result on the boot console. It is the one place the `TAIRiX <version>
//! <RAM>MiB` identity line is drawn: the figure starts at zero and climbs to
//! the installed total as each region of RAM is proven, so the operator
//! watches the machine verify its own memory before a single frame is handed
//! out.
//!
//! * While the test runs, the byte count of *verified* RAM is redrawn in
//!   place (a leading carriage return) with the number in **yellow** — RAM
//!   being verified, but not yet fully proven.
//! * When it completes, the line settles on the installed total, redrawn in
//!   **light green** — the RAM has passed.
//! * If a fault is found, the number is redrawn in **red** as the MiB offset
//!   of the failing location, a diagnostic line states the physical address
//!   and the mismatch, and the boot is **halted** — TAIRiX never runs on RAM
//!   it could not trust (fail closed, fail loud).
//!
//! The colour is applied to the *number only*, never the `MiB` unit, so the
//! unit stays legible on a monochrome serial line while the figure carries
//! the pass/fail signal on a colour console.

use core::fmt::Write as _;

use tairix_util::fmt::format_hex_u64;

use crate::bootinfo::KernelArch;
use crate::console::ConsoleDevice;

/// One binary mebibyte — the unit the counter is shown in.
const MIB: u64 = 1024 * 1024;

/// Upper bound on in-place counter redraws over the whole test.
///
/// The engine reports progress far more often than this (every couple of
/// MiB); the driver coalesces those into at most this many on-screen updates,
/// spread evenly across installed RAM, so the animation is smooth on a small
/// machine yet never pays thousands of framebuffer blits on a large one.
const PROGRESS_REDRAWS: u64 = 256;

/// The identity prefix, `TAIRiX <version>`, drawn ahead of the RAM figure.
///
/// The version is the workspace crate version stamped in at compile time, so
/// the banner stays bit-reproducible (no build clock). This is the single
/// on-screen owner of the `TAIRiX <version> <RAM>MiB` line — userland `init`
/// prints only the machine-summary line beneath it, never a second copy.
const BANNER_PREFIX: &str = concat!("TAIRiX ", env!("CARGO_PKG_VERSION"));

/// Select-graphic-rendition escape for the in-progress figure while the
/// test is still running (bright yellow): the RAM is being verified but not
/// yet proven, so it is neither the pass green nor the fault red.
const SGR_PROGRESS: &str = "\x1b[93m";
/// Select-graphic-rendition escape for the passed/verified figure
/// (bright — "light" — green).
const SGR_PASS: &str = "\x1b[92m";
/// Select-graphic-rendition escape for a failing figure (bright red).
const SGR_FAIL: &str = "\x1b[91m";
/// Reset select-graphic-rendition back to the console default.
const SGR_RESET: &str = "\x1b[0m";

/// Capacity of a single rendered banner line.
///
/// Sized for the longest output — the two-line fault report: the coloured
/// counter, then a diagnostic naming three `0x`-prefixed 64-bit values and
/// the fixed wording. 256 bytes clears that comfortably; a formatting
/// overflow is impossible for these inputs and fails closed (the partial
/// line is dropped) rather than corrupting the console.
const LINE_MAX: usize = 256;

/// A bounded, allocation-free `core::fmt::Write` sink over a stack buffer.
///
/// Refuses (fails closed) any write past the buffer's end rather than
/// truncating mid-escape, so a colour sequence is never split.
struct Line {
    buf: [u8; LINE_MAX],
    len: usize,
}

impl Line {
    fn new() -> Self {
        Self {
            buf: [0; LINE_MAX],
            len: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl core::fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let end = self.len.checked_add(bytes.len()).ok_or(core::fmt::Error)?;
        if end > self.buf.len() {
            return Err(core::fmt::Error);
        }
        self.buf[self.len..end].copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }
}

/// Render the in-progress / final counter line: `\rTAIRiX <v> <N>MiB`.
///
/// The figure is drawn in **yellow** while the test is still running (the RAM
/// is being verified but not yet proven) and in **light green** once every
/// region has passed. `newline` appends a line feed only on that final,
/// completed line so the next console output (userland `init`'s machine
/// summary) starts on its own line; while the test runs the line is left open
/// for the next in-place redraw.
fn counter_line(mib: u64, complete: bool) -> Line {
    let colour = if complete { SGR_PASS } else { SGR_PROGRESS };
    let mut line = Line::new();
    let _ = write!(line, "\r{BANNER_PREFIX} {colour}{mib}{SGR_RESET}MiB");
    if complete {
        let _ = line.write_str("\n");
    }
    line
}

/// Render the fault line: the failing MiB offset in red, then a diagnostic
/// line naming the physical address and the mismatch. Fail loud.
fn fault_lines(fault: tairix_kernel_mem::RamFault) -> Line {
    let mib = fault.phys.as_u64() / MIB;
    let mut line = Line::new();
    let _ = write!(line, "\r{BANNER_PREFIX} {SGR_FAIL}{mib}{SGR_RESET}MiB");
    let mut phys = [0u8; 16];
    let mut expected = [0u8; 16];
    let mut observed = [0u8; 16];
    let _ = writeln!(
        line,
        "\nRAM self-test FAILED at physical {} (expected {}, read {}); halting.",
        format_hex_u64(fault.phys.as_u64(), &mut phys),
        format_hex_u64(fault.expected, &mut expected),
        format_hex_u64(fault.observed, &mut observed),
    );
    line
}

/// Write `bytes` to every boot console, best-effort: loop over benign short
/// writes and stop on a console that accepts nothing more or errors. The
/// counter lands on *all* consoles — the framebuffer screen and the serial
/// line alike — so neither a graphical nor a headless operator misses the
/// machine proving its own RAM. The boot proceeds whether or not the banner
/// fully lands on any console — it is diagnostic, not a gate.
fn emit(consoles: &[ConsoleDevice], bytes: &[u8]) {
    for console in consoles {
        let mut offset = 0;
        while offset < bytes.len() {
            match console.write_output(&bytes[offset..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => offset += n,
            }
        }
    }
}

/// Record what the self-test actually proved.
///
/// The engine leaves a span the direct map does not cover untested rather
/// than trusting it, which the on-screen counter cannot show: it settles on
/// the machine's advertised size either way. Unreachable RAM means the
/// kernel cannot address some of its own memory by pointer — every consumer
/// that draws such a frame will fail closed — so it is recorded at `Warn`
/// with both totals rather than passing silently.
fn log_selftest(sink: &(dyn tairix_log::Sink + Sync), totals: tairix_kernel_mem::RamTestTotals) {
    use tairix_log::{Event, Field, FieldValue, Level};

    let (level, message) = if totals.unreachable == 0 {
        (Level::Info, "ram self-test verified every usable byte")
    } else {
        (
            Level::Warn,
            "ram self-test left usable RAM untested: outside the direct map",
        )
    };
    tairix_log::log(
        sink,
        &Event {
            level,
            id: crate::AuditEvent::RamSelfTest.id(),
            message,
            fields: &[
                Field {
                    key: "verified_bytes",
                    value: FieldValue::UnsignedInt(totals.verified),
                },
                Field {
                    key: "unreachable_bytes",
                    value: FieldValue::UnsignedInt(totals.unreachable),
                },
            ],
        },
    );
}

/// Run the early-boot RAM self-test and display its progress on every boot
/// console in `consoles`.
///
/// Tests every usable region of `map` through the port's direct physical map
/// before the frame allocator hands out a frame, drawing the verified-MiB
/// counter on each console as it goes — the framebuffer screen and the serial
/// line alike, so a headless boot shows the same identity/RAM line a
/// graphical one does. On success it returns and the boot continues, having
/// left every tested region zeroed. On a fault it draws the failing location
/// in red and calls [`KernelArch::halt`], which never returns.
///
/// A port with no direct physical map (the host/`wasm32` environment) cannot
/// reach physical RAM to test it; the self-test is skipped rather than faked,
/// and the boot continues.
pub fn run<A: KernelArch>(
    arch: &A,
    map: &tairix_kernel_mem::BootMemoryMap,
    installed_bytes: u64,
    consoles: &[ConsoleDevice],
    log_sink: &(dyn tairix_log::Sink + Sync),
) {
    let Some(physmap) = arch.direct_phys_map() else {
        return;
    };

    // Open the line at zero: the machine starts unverified.
    emit(consoles, counter_line(0, false).as_bytes());

    // The engine reports progress every couple of MiB, which on a
    // many-gigabyte machine is thousands of steps — far more in-place
    // redraws than an operator can see and, on a framebuffer console, a real
    // per-step blit cost. Throttle to a bounded number of updates spread
    // across the whole test (`PROGRESS_REDRAWS`), so the counter animates
    // just as smoothly on 256 MiB as on 64 GiB without the redraw dominating
    // the test's run time.
    let redraw_bytes = (installed_bytes / PROGRESS_REDRAWS).max(MIB);
    let mut next_redraw = 0u64;
    let outcome = tairix_kernel_mem::ram_selftest(map, physmap, |verified_bytes| {
        if verified_bytes >= next_redraw {
            emit(
                consoles,
                counter_line(verified_bytes / MIB, false).as_bytes(),
            );
            next_redraw = verified_bytes.saturating_add(redraw_bytes);
        }
    });

    match outcome {
        Ok(totals) => {
            log_selftest(log_sink, totals);
            // Settle the line on the installed total (rounded to the nearest
            // MiB, matching the machine's advertised size) and close it. When
            // the port reported no installed figure, show the verified total
            // rather than a bare zero — never a figure the test did not earn.
            let mib = if installed_bytes > 0 {
                installed_bytes.saturating_add(MIB / 2) / MIB
            } else {
                totals.verified / MIB
            };
            emit(consoles, counter_line(mib, true).as_bytes());
        }
        Err(fault) => {
            emit(consoles, fault_lines(fault).as_bytes());
            arch.halt();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_kernel_mem::{PhysAddr, RamFault};

    fn text(line: &Line) -> &str {
        core::str::from_utf8(line.as_bytes()).expect("ascii banner")
    }

    #[test]
    fn in_progress_counter_line_colours_only_the_number_yellow() {
        let line = counter_line(4096, false);
        let s = text(&line);
        assert!(s.starts_with('\r'), "redraws in place");
        assert!(s.contains(BANNER_PREFIX));
        // While the test runs the number is wrapped in the yellow escape and
        // reset, and the `MiB` unit sits *outside* the colouring.
        assert!(s.contains(&std::format!("{SGR_PROGRESS}4096{SGR_RESET}MiB")));
        assert!(!s.ends_with('\n'), "an in-progress line stays open");
    }

    #[test]
    fn final_counter_line_colours_the_number_light_green_and_closes_it() {
        let line = counter_line(256, true);
        let s = text(&line);
        // The completed figure is light green (proven RAM), never yellow, and
        // the line is closed with a newline for the following summary.
        assert!(s.contains(&std::format!("{SGR_PASS}256{SGR_RESET}MiB")));
        assert!(!s.contains(SGR_PROGRESS), "the final line is not yellow");
        assert!(s.ends_with("MiB\n"));
    }

    #[test]
    fn fault_line_shows_the_failing_mib_in_red_and_states_the_address() {
        let fault = RamFault {
            phys: PhysAddr::new(3 * MIB + 0x40),
            expected: 0xAAAA_AAAA_AAAA_AAAA,
            observed: 0x8AAA_AAAA_AAAA_AAAA,
        };
        let line = fault_lines(fault);
        let s = text(&line);
        // The failing location is 3 MiB, shown in red.
        assert!(s.contains(&std::format!("{SGR_FAIL}3{SGR_RESET}MiB")));
        assert!(s.contains("RAM self-test FAILED"));
        assert!(s.contains("halting"));
    }

    #[test]
    fn emit_to_no_console_is_a_silent_no_op() {
        // Simply must not panic when there are no consoles.
        emit(&[], b"ignored");
    }
}
