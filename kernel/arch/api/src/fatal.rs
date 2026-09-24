//! The fatal report: the two records a dying kernel ends with, and the one
//! writer the ports share for the reports their own paths make.
//!
//! Every fatal report — the kernel's full post-mortem (`kernel/core`) and a
//! port's own report below — ends with exactly one record in the diagnostic
//! line shape (`lib/log`): [`KERNEL_PANIC`] or [`KERNEL_FAULT`]. Whatever reads
//! the console, a person or the QEMU harness, therefore has one line to find
//! and knows nothing of the report follows it.
//!
//! A port writes its own report where no kernel stands behind it: a minimal
//! test kernel that links only the port, or a fault taken before the kernel
//! installed its fault handler. Those reports carry the cause, the processor,
//! and the boot-stack guard's verdict; the register snapshot and backtrace are
//! the kernel's post-mortem to add.

use core::fmt::{self, Write};
use core::panic::Location;

use tairix_log::{write_diag_line, Event, EventId, Field, FieldValue, Level};

use crate::backtrace::BootStackGuard;

/// One of the two records a fatal report ends with.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FatalRecord {
    /// The record's stable event id.
    pub id: EventId,
    /// The record's fixed message.
    pub message: &'static str,
}

/// The record a kernel panic ends with.
pub const KERNEL_PANIC: FatalRecord = FatalRecord {
    id: EventId(4010),
    message: "kernel panic",
};

/// The record a fatal kernel-mode CPU exception ends with.
pub const KERNEL_FAULT: FatalRecord = FatalRecord {
    id: EventId(4011),
    message: "fatal kernel fault",
};

/// Bytes [`format_hex_word`] writes: `0x` and sixteen nibbles.
pub const HEX_WORD_LEN: usize = 18;

/// Render `value` into `buf` as `0x` and sixteen lowercase nibbles, and
/// return the whole buffer: the fixed width a fatal record spells a register
/// or an address in, so a column of them stays aligned.
#[must_use]
pub fn format_hex_word(value: u64, buf: &mut [u8; HEX_WORD_LEN]) -> &str {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    buf[0] = b'0';
    buf[1] = b'x';
    let mut rest = value;
    for slot in buf[2..].iter_mut().rev() {
        *slot = DIGITS[(rest & 0xf) as usize];
        rest >>= 4;
    }
    core::str::from_utf8(&buf[..]).unwrap_or("0x")
}

/// A fatal CPU exception taken in **kernel** mode, as the port's
/// synchronous-exception vector saw it.
///
/// The same three words on every port, spelled differently per
/// architecture: `ESR_EL1` / `FAR_EL1` / `ELR_EL1` on aarch64, the packed
/// vector and error code / faulting linear address / `RIP` on x86_64, and
/// `scause` / `stval` / `sepc` on riscv64. The port names them; everything
/// above it reads the neutral triple.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct KernelFault {
    /// The port's exception syndrome — why the CPU trapped.
    pub syndrome: u64,
    /// The address the faulting access could not reach.
    pub address: u64,
    /// The faulting instruction.
    pub pc: u64,
}

impl fmt::Display for KernelFault {
    /// One line, hex, in the field order the fatal record uses, so a prose
    /// report and the record read alike.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "syndrome={:#018x} fault_addr={:#018x} fault_pc={:#018x}",
            self.syndrome, self.address, self.pc
        )
    }
}

/// How a port names itself and one of its processors in a report.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Reporter {
    /// The port, as the report's cause line names it (`aarch64`).
    pub port: &'static str,
    /// What the port calls one processor (`CPU`, `hart`).
    pub unit: &'static str,
}

impl Reporter {
    /// Write the report of a panic on processor `cpu`, ending with its
    /// [`KERNEL_PANIC`] record.
    ///
    /// `message` is what the panic said — a port passes its `PanicInfo`,
    /// whose display carries the message and the location — and `location`
    /// is where it was raised, for the record. `guard` is the boot-stack
    /// guard's verdict, where the port reserves one: a panic whose real cause
    /// was an overrun otherwise reads as an unexplained corruption of whatever
    /// sat below the stack.
    pub fn panic(
        self,
        w: &mut dyn Write,
        cpu: u32,
        message: &dyn fmt::Display,
        location: Option<&Location<'_>>,
        guard: Option<BootStackGuard>,
    ) {
        self.banner(
            w,
            "PANIC",
            cpu,
            format_args!("{} panic on {} {cpu}: {message}", self.port, self.unit),
            guard,
        );
        let (file, line, column) = location.map_or(("<unknown>", 0, 0), |at| {
            (at.file(), at.line(), at.column())
        });
        record(
            w,
            KERNEL_PANIC,
            cpu,
            &[
                Field {
                    key: "file",
                    value: FieldValue::Str(file),
                },
                Field {
                    key: "line",
                    value: FieldValue::UnsignedInt(u64::from(line)),
                },
                Field {
                    key: "column",
                    value: FieldValue::UnsignedInt(u64::from(column)),
                },
            ],
            guard,
        );
    }

    /// Write the report of a fatal exception no fault handler claimed on
    /// processor `cpu`, ending with its [`KERNEL_FAULT`] record.
    ///
    /// `decoded` is the port's own reading of the three words — the register
    /// names, the exception class, the privilege it was taken at — which the
    /// neutral record cannot carry.
    pub fn fault(
        self,
        w: &mut dyn Write,
        cpu: u32,
        fault: KernelFault,
        decoded: fmt::Arguments<'_>,
        guard: Option<BootStackGuard>,
    ) {
        self.banner(
            w,
            "FAULT",
            cpu,
            format_args!(
                "{} exception on {} {cpu} with no fault handler installed: {decoded}",
                self.port, self.unit
            ),
            guard,
        );
        let mut syndrome = [0u8; HEX_WORD_LEN];
        let mut address = [0u8; HEX_WORD_LEN];
        let mut pc = [0u8; HEX_WORD_LEN];
        record(
            w,
            KERNEL_FAULT,
            cpu,
            &[
                Field {
                    key: "syndrome",
                    value: FieldValue::Str(format_hex_word(fault.syndrome, &mut syndrome)),
                },
                Field {
                    key: "fault_addr",
                    value: FieldValue::Str(format_hex_word(fault.address, &mut address)),
                },
                Field {
                    key: "fault_pc",
                    value: FieldValue::Str(format_hex_word(fault.pc, &mut pc)),
                },
            ],
            guard,
        );
    }

    /// The prose half of a report, for a person reading the console.
    fn banner(
        self,
        w: &mut dyn Write,
        kind: &str,
        cpu: u32,
        cause: fmt::Arguments<'_>,
        guard: Option<BootStackGuard>,
    ) {
        let _ = writeln!(
            w,
            "\n==================== TAIRiX KERNEL {kind} ===================="
        );
        let _ = writeln!(w, "[tairix-kernel] {cause}");
        if let Some(verdict) = guard {
            let _ = writeln!(w, "boot-stack guard: {verdict}");
        }
        let _ = writeln!(
            w,
            "{} {cpu} halted; the kernel is non-recoverable in production.",
            self.unit
        );
        let _ = writeln!(
            w,
            "============================================================="
        );
    }
}

/// Write `kind`'s record: the processor, the cause fields, and the guard's
/// verdict, in the keys the kernel's own post-mortem uses.
fn record(
    w: &mut dyn Write,
    kind: FatalRecord,
    cpu: u32,
    cause: &[Field<'_>; 3],
    guard: Option<BootStackGuard>,
) {
    let mut overrun = [0u8; HEX_WORD_LEN];
    let [first, second, third] = *cause;
    let mut fields = [
        Field {
            key: "cpu",
            value: FieldValue::UnsignedInt(u64::from(cpu)),
        },
        first,
        second,
        third,
        Field {
            key: "boot_stack_guard",
            value: FieldValue::Null,
        },
        Field {
            key: "boot_stack_overrun_bytes",
            value: FieldValue::Null,
        },
    ];
    let used = match guard {
        None => 4,
        Some(verdict @ BootStackGuard::BelowStack { bytes, .. }) => {
            fields[4].value = FieldValue::Str(verdict.label());
            fields[5].value = FieldValue::Str(format_hex_word(bytes, &mut overrun));
            6
        }
        Some(verdict) => {
            fields[4].value = FieldValue::Str(verdict.label());
            5
        }
    };
    write_diag_line(
        w,
        None,
        false,
        &Event {
            level: Level::Error,
            id: kind.id,
            message: kind.message,
            fields: &fields[..used],
        },
    );
}

#[cfg(test)]
#[path = "fatal_tests.rs"]
mod tests;
