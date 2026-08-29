//! riscv64 U-mode fixture: fill the whole floating-point register file with a
//! per-task pattern, trap into the kernel, and check every register came back.
//!
//! The register file is task state the riscv64 port did not switch
//! (`plans/OPEN-DEFECTS.md` D37): firmware hands S-mode `sstatus.FS = Dirty`,
//! so floating point runs freely, while no `fsd`/`fld` existed anywhere in the
//! port. Two tasks therefore shared one physical file — each reading whatever
//! the last one left in it, which is disclosure as much as corruption. The
//! consuming vertical (`tests/integration/fp_isolation_qemu_riscv64`) runs two
//! copies of this program with different patterns interleaved on one hart; it
//! passes only if neither sees the other's values.
//!
//! The load, the trap, and the read-back are **one** asm block on purpose. Half
//! the FP registers are caller-saved, so a Rust call between them would let the
//! compiler treat the values as dead, and the test could pass without ever
//! having held them across the trap.
//!
//! It is a **pure-Rust** program: it links `tairix-rt` (which provides
//! `_start`, the stack canary, the panic handler and the syscall wrappers),
//! never the C ABI. On the host, and on any target that is not freestanding
//! riscv64, it is an inert stub so `cargo build --workspace`, clippy and fmt
//! still cover the crate.

#![cfg_attr(fp_probe, no_std)]
#![cfg_attr(fp_probe, no_main)]
#![deny(missing_docs)]

// --- riscv64 U-mode program ---------------------------------------------
#[cfg(fp_probe)]
mod program {
    use tairix_abi::SyscallNumber;

    /// Floating-point registers the RISC-V ISA defines.
    const FP_REGS: usize = 32;

    /// Trips the pattern apart per register, so a save/restore that transposed,
    /// truncated, or only partly covered the file fails rather than passing on
    /// a coincidence.
    const SPREAD: u64 = 0x9E37_79B9_7F4A_7C15;

    /// Exit code for a register that came back holding something else.
    const EXIT_FILE_CLOBBERED: i32 = 72;
    /// Exit code for a missing or unusable per-task seed argument.
    const EXIT_NO_SEED: i32 = 73;

    /// Rounds of fill-trap-verify when the consuming build pinned no count.
    /// More than one so a port that happened to preserve the file across the
    /// *first* trap still fails.
    const DEFAULT_ROUNDS: u32 = 4;

    /// The round count, read from the `TAIRIX_FP_ROUNDS` environment variable
    /// the consuming vertical's build script sets when it compiles this
    /// program. That script emits the same number as a Rust constant for its
    /// kernel side, so it is the single source of truth for the yield count
    /// the vertical asserts against.
    const fn rounds() -> u32 {
        match option_env!("TAIRIX_FP_ROUNDS") {
            Some(text) => parse_u32(text.as_bytes()),
            None => DEFAULT_ROUNDS,
        }
    }

    /// Parse `bytes` as a non-negative decimal integer at compile time,
    /// falling back to [`DEFAULT_ROUNDS`] on an empty string, a non-digit byte,
    /// or overflow. `const` and panic-free, so the count is fixed into the
    /// image with no runtime parsing (fail closed to the default).
    const fn parse_u32(bytes: &[u8]) -> u32 {
        let mut acc: u32 = 0;
        let mut index = 0usize;
        let mut seen = false;
        while index < bytes.len() {
            let byte = bytes[index];
            if byte < b'0' || byte > b'9' {
                return DEFAULT_ROUNDS;
            }
            acc = match acc.checked_mul(10) {
                Some(scaled) => match scaled.checked_add((byte - b'0') as u32) {
                    Some(next) => next,
                    None => return DEFAULT_ROUNDS,
                },
                None => return DEFAULT_ROUNDS,
            };
            seen = true;
            index += 1;
        }
        if seen {
            acc
        } else {
            DEFAULT_ROUNDS
        }
    }

    /// The value register `index` carries for this task in `round`.
    ///
    /// Every pattern is a quiet-NaN payload rather than a signalling encoding:
    /// the fixture only moves bits, and a trap on a signalling NaN would be
    /// this test's own bug rather than the kernel's.
    fn pattern(seed: u64, round: u64, index: usize) -> u64 {
        let index = index as u64;
        let base = 0x7FF8_0000_0000_0000 | (seed << 32) | (round << 16) | (index & 0xFFFF);
        base ^ (SPREAD.wrapping_mul(index + 1) & 0x0000_FFFF_FFFF_0000)
    }

    /// Fill `f0`–`f31` from `write`, yield, then read them back into `read`.
    ///
    /// Packed several per line only so the whole file fits one readable block;
    /// the order and the offsets are what matter.
    fn hold_across_trap(write: &[u64; FP_REGS], read: &mut [u64; FP_REGS]) {
        let number = u64::from(SyscallNumber::YIELD.as_u16());
        // SAFETY: both operands are 32-doubleword buffers the caller owns, so
        // every load and store stays inside them. `ecall` is the `abi-v1`
        // trap: the kernel reads the number from `a7`, and `yield` is
        // unprivileged, takes no argument, and resumes at the next
        // instruction. Declaring `f0`–`f31` as written tells the compiler the
        // block owns the whole file, which is the point of the fixture.
        unsafe {
            core::arch::asm!(
                "fld f0, 0({w})", "fld f1, 8({w})", "fld f2, 16({w})", "fld f3, 24({w})", "fld f4, 32({w})", "fld f5, 40({w})",
                "fld f6, 48({w})", "fld f7, 56({w})", "fld f8, 64({w})", "fld f9, 72({w})", "fld f10, 80({w})", "fld f11, 88({w})",
                "fld f12, 96({w})", "fld f13, 104({w})", "fld f14, 112({w})", "fld f15, 120({w})", "fld f16, 128({w})", "fld f17, 136({w})",
                "fld f18, 144({w})", "fld f19, 152({w})", "fld f20, 160({w})", "fld f21, 168({w})", "fld f22, 176({w})", "fld f23, 184({w})",
                "fld f24, 192({w})", "fld f25, 200({w})", "fld f26, 208({w})", "fld f27, 216({w})", "fld f28, 224({w})", "fld f29, 232({w})",
                "fld f30, 240({w})", "fld f31, 248({w})",
                "ecall",
                "fsd f0, 0({r})", "fsd f1, 8({r})", "fsd f2, 16({r})", "fsd f3, 24({r})", "fsd f4, 32({r})", "fsd f5, 40({r})",
                "fsd f6, 48({r})", "fsd f7, 56({r})", "fsd f8, 64({r})", "fsd f9, 72({r})", "fsd f10, 80({r})", "fsd f11, 88({r})",
                "fsd f12, 96({r})", "fsd f13, 104({r})", "fsd f14, 112({r})", "fsd f15, 120({r})", "fsd f16, 128({r})", "fsd f17, 136({r})",
                "fsd f18, 144({r})", "fsd f19, 152({r})", "fsd f20, 160({r})", "fsd f21, 168({r})", "fsd f22, 176({r})", "fsd f23, 184({r})",
                "fsd f24, 192({r})", "fsd f25, 200({r})", "fsd f26, 208({r})", "fsd f27, 216({r})", "fsd f28, 224({r})", "fsd f29, 232({r})",
                "fsd f30, 240({r})", "fsd f31, 248({r})",
                w = in(reg) write.as_ptr(),
                r = in(reg) read.as_mut_ptr(),
                in("a7") number,
                out("a0") _,
                out("f0") _, out("f1") _, out("f2") _, out("f3") _, out("f4") _, out("f5") _, out("f6") _, out("f7") _,
                out("f8") _, out("f9") _, out("f10") _, out("f11") _, out("f12") _, out("f13") _, out("f14") _, out("f15") _,
                out("f16") _, out("f17") _, out("f18") _, out("f19") _, out("f20") _, out("f21") _, out("f22") _, out("f23") _,
                out("f24") _, out("f25") _, out("f26") _, out("f27") _, out("f28") _, out("f29") _, out("f30") _, out("f31") _,
                options(nostack)
            );
        }
    }

    /// Program entry point.
    fn main() -> i32 {
        // The vertical gives each task a one-character seed, so the two runs
        // hold patterns that cannot be mistaken for one another. Read through
        // the borrowing accessor rather than `args()`, whose `Vec` would take
        // the heap — and with it a `mem_map` syscall this fixture has no reason
        // to issue.
        let count = tairix_rt::arg_count();
        let Some(seed) = count
            .checked_sub(1)
            .and_then(tairix_rt::arg)
            .and_then(|bytes| bytes.first().copied())
        else {
            return EXIT_NO_SEED;
        };
        let seed = u64::from(seed);

        for round in 0..u64::from(rounds()) {
            let mut write = [0u64; FP_REGS];
            for (index, slot) in write.iter_mut().enumerate() {
                *slot = pattern(seed, round, index);
            }
            let mut read = [0u64; FP_REGS];
            hold_across_trap(&write, &mut read);
            if read != write {
                return EXIT_FILE_CLOBBERED;
            }
        }
        0
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) and on every target
// that is not freestanding riscv64 the program body is not compiled, so this
// inert `main` keeps the crate building under the host tooling. It performs no
// I/O.
#[cfg(not(fp_probe))]
fn main() {}
