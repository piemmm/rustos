//! x86_64 exception entries: one stub per architecturally-defined vector,
//! funnelled into a single fatal tail.
//!
//! `percpu::init` populates every IDT slot with the vector-agnostic
//! fail-closed thunk ([`crate::interrupts`]), which cannot say *which*
//! exception fired and cannot read the error code the CPU pushes for the
//! subset of vectors that push one. This module gives each vector its own
//! stub — the vector as an immediate, the hardware error code where the
//! CPU pushed one, a synthetic zero where it did not — so a kernel-mode
//! `#GP`, `#UD`, `#DF` or machine check reaches the installed
//! [`crate::fault::FaultHandlerFn`] and states its cause, instead of
//! parking mutely.
//!
//! It is the x86_64 counterpart of the aarch64
//! `exceptions::fatal_exception` / riscv64 `trap::fatal_exception` tails:
//! every unhandled exception funnels into `fatal_exception`, which
//! reports through the one fatal policy the boot path installs.
//!
//! # The vector table
//!
//! Intel SDM Vol 3A Table 6-1 defines vectors `0..=21`; `22..=31` are
//! reserved but architecturally still exceptions, so they get stubs too
//! (a delivery on one is a real fault, not something to ignore). The
//! vectors the CPU pushes a hardware error code for are `8`, `10`–`14`,
//! `17` and `21` (SDM Vol 3A §6.13).
//!
//! Vector 14 (`#PF`) is deliberately **absent**: a page fault is the one
//! exception this kernel can resolve — a demand-paged file mapping, or a
//! fault inside the guarded user-copy window — so it keeps its own
//! resumable entry in [`crate::fault`] and is installed separately by the
//! boot path.
//!
//! # Faults taken from ring 3
//!
//! A ring-3 exception is the running task's fault, not the CPU's, and
//! should cost only that task — which is what the aarch64 and riscv64
//! tails do through their user-fault terminator. This port has no such
//! terminator yet, so a ring-3 exception other than `#PF` still reaches
//! the fatal report; the report's syndrome records that it came from
//! ring 3 ([`crate::fault::syndrome_from_user`]) rather than claiming a
//! kernel fault, and wiring the terminator is tracked in
//! `plans/OPEN-DEFECTS.md` D86.

/// Highest IDT vector this module owns; it owns every vector from `0` up
/// to this one. Above `31` is the user-defined interrupt range, not
/// exceptions (Intel SDM Vol 3A §6.3.1).
const LAST_EXCEPTION_VECTOR: u8 = 31;

/// Ring the exception was taken from, decoded from the saved `CS`.
///
/// The low two bits of a code selector are its RPL, which for the saved
/// `CS` of an interrupt frame is the CPL the CPU was running at (Intel
/// SDM Vol 3A §6.14.2). Anything but `0` is user mode.
const CPL_MASK: u64 = 0b11;

/// `true` when the saved `CS` says the exception came from ring 3.
#[must_use]
pub const fn cs_is_user(cs: u64) -> bool {
    cs & CPL_MASK != 0
}

/// The one fatal tail every exception stub funnels into.
///
/// Packs the vector, the hardware error code and the originating
/// privilege level into the neutral syndrome word
/// ([`crate::fault::exception_syndrome`]) and hands it to the installed
/// fatal handler, which records one `KernelFault` audit line and halts.
///
/// The faulting address is reported as `0`: outside `#PF` no x86_64
/// exception supplies one, and `CR2` would name whichever page fault
/// happened *last* — a fabricated field is worse than an absent one, so
/// the syndrome names the vector and the address field stays empty.
///
/// With no handler installed — a window the boot path closes before the
/// IDT can deliver anything — the CPU parks. Never a silent reset, and
/// never through QEMU's debug-exit port: a fatal decision in a production
/// kernel does not run through a test-harness affordance.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn fatal_exception(vector: u8, error_code: u64, rip: u64, cs: u64) -> ! {
    let syndrome = crate::fault::exception_syndrome(vector, error_code, cs_is_user(cs));
    if let Some(handler) = crate::fault::fault_handler() {
        handler(syndrome, 0, rip);
    }
    crate::reset::park_cpu()
}

/// Rust dispatcher every generated exception stub calls.
///
/// The arguments are the stub's marshalling of `(vector immediate,
/// hardware error code or a synthetic zero, faulting `rip`, saved `cs`)`.
/// Diverges, so the stubs need no epilogue.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[no_mangle]
extern "C" fn tairix_arch_x86_64_exception_dispatch(
    vector: u64,
    error_code: u64,
    rip: u64,
    cs: u64,
) -> ! {
    // The stub passes its own `const` vector, which the generator bounds
    // to `0..=31`; mask rather than widen the report's vector field on a
    // value that cannot occur.
    #[allow(clippy::cast_possible_truncation)]
    // SAFETY-INVARIANT: masked to 8 bits, so the narrowing is lossless.
    let vector = (vector & 0xFF) as u8;
    fatal_exception(vector, error_code, rip, cs)
}

/// Declare every exception vector's stub and the table the installer
/// walks, so a vector is written once and cannot drift between the two.
///
/// `=> error_code` marks a vector the CPU pushes a hardware error code
/// for.
macro_rules! exception_vectors {
    ( $( $vector:literal => $name:ident $( => $err:ident )? ),* $(,)? ) => {
        $(
            $crate::define_exception_isr!(
                $name => tairix_arch_x86_64_exception_dispatch,
                vector = $vector
                $( , $err )?
            );
        )*

        /// Every owned vector paired with its stub entry point.
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        const EXCEPTION_STUBS: &[(u8, unsafe extern "C" fn())] = &[
            $( ($vector, $name) ),*
        ];

        /// Every owned vector and whether the CPU pushes a hardware error
        /// code for it. Compiled on every configuration, so the table's
        /// coverage is both build-time asserted and host-unit-tested
        /// without the freestanding stubs.
        const EXCEPTION_VECTOR_SHAPES: &[(u8, bool)] = &[
            $( ($vector, exception_vectors!(@has_err $( $err )?)) ),*
        ];
    };
    (@has_err error_code) => { true };
    (@has_err) => { false };
}

// Intel SDM Vol 3A Table 6-1. Vector 14 (`#PF`) is absent: it keeps the
// resumable entry in `crate::fault`.
exception_vectors! {
    0 => isr_divide_error,
    1 => isr_debug,
    2 => isr_nmi,
    3 => isr_breakpoint,
    4 => isr_overflow,
    5 => isr_bound_range,
    6 => isr_invalid_opcode,
    7 => isr_device_not_available,
    8 => isr_double_fault => error_code,
    9 => isr_coprocessor_segment_overrun,
    10 => isr_invalid_tss => error_code,
    11 => isr_segment_not_present => error_code,
    12 => isr_stack_segment_fault => error_code,
    13 => isr_general_protection => error_code,
    15 => isr_reserved_15,
    16 => isr_fpu_error,
    17 => isr_alignment_check => error_code,
    18 => isr_machine_check,
    19 => isr_simd_fp,
    20 => isr_virtualisation,
    21 => isr_control_protection => error_code,
    22 => isr_reserved_22,
    23 => isr_reserved_23,
    24 => isr_reserved_24,
    25 => isr_reserved_25,
    26 => isr_reserved_26,
    27 => isr_reserved_27,
    28 => isr_reserved_28,
    29 => isr_reserved_29,
    30 => isr_reserved_30,
    31 => isr_reserved_31,
}

/// A vector outside the exception range, or the `#PF` this module must
/// not claim, is a typo in the table above — catch it at build time rather
/// than by installing a gate over the resumable page-fault entry or over
/// a device's interrupt vector.
const _: () = {
    let mut index = 0;
    while index < EXCEPTION_VECTOR_SHAPES.len() {
        let (vector, _) = EXCEPTION_VECTOR_SHAPES[index];
        assert!(vector <= LAST_EXCEPTION_VECTOR);
        assert!(vector != crate::fault::PAGE_FAULT_VECTOR);
        index += 1;
    }
};

/// Install every exception vector's dedicated stub in `cpu_index`'s
/// per-CPU IDT, replacing the vector-agnostic default thunk
/// `crate::percpu::init` left there.
///
/// Called on every CPU as it comes online, immediately after
/// `percpu::init` and before anything can fault. Vector 14 is untouched
/// — the boot path installs the resumable `#PF` entry over it — and the
/// `#DF` / `#NMI` gates keep the IST routing `percpu::init` chose, because
/// `install_vector` derives it from the same shared mapping.
///
/// # Errors
///
/// * [`crate::percpu::InitError::CpuIndexOutOfRange`] if `cpu_index` is
///   outside the registered `PerCpuStorage`.
/// * [`crate::percpu::InitError::NotInitialised`] if `crate::percpu::init`
///   has not yet run for `cpu_index`.
///
/// An error leaves the vectors installed so far installed and the rest on
/// the default thunk. That partial table never runs: the boot path refuses
/// the boot on a refusal, and every slot in it is one of the two
/// fail-closed entries either way — so the honest posture is "some
/// vectors report, the rest park", not a corrupt table.
///
/// # Safety
///
/// * `cpu_index` must be the index passed to `crate::percpu::init` on
///   *this* CPU.
/// * Interrupts on the calling CPU must be disabled for the duration, so
///   a delivery cannot race an IDT write.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub unsafe fn install_exception_vectors(cpu_index: usize) -> Result<(), crate::percpu::InitError> {
    for &(vector, stub) in EXCEPTION_STUBS {
        let handler = stub as *const () as usize as u64;
        // SAFETY: the caller's contract gives us this CPU's own index with
        // interrupts disabled, and `handler` is the address of a generated
        // stub in this image.
        unsafe { crate::percpu::install_vector(cpu_index, vector, handler)? };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fault::PAGE_FAULT_VECTOR;

    /// The vectors Intel SDM Vol 3A §6.13 says push a hardware error
    /// code. `#PF` (14) is on that list but is not this module's vector.
    const ERROR_CODE_VECTORS: &[u8] = &[8, 10, 11, 12, 13, 17, 21];

    #[test]
    fn every_exception_vector_but_the_page_fault_has_a_stub() {
        for vector in 0..=LAST_EXCEPTION_VECTOR {
            let present = EXCEPTION_VECTOR_SHAPES.iter().any(|&(v, _)| v == vector);
            assert_eq!(
                present,
                vector != PAGE_FAULT_VECTOR,
                "vector {vector} coverage"
            );
        }
    }

    #[test]
    fn the_error_code_marker_matches_the_intel_sdm() {
        for &(vector, has_error_code) in EXCEPTION_VECTOR_SHAPES {
            assert_eq!(
                has_error_code,
                ERROR_CODE_VECTORS.contains(&vector),
                "vector {vector} error-code shape"
            );
        }
    }

    #[test]
    fn no_vector_is_declared_twice() {
        for (index, &(vector, _)) in EXCEPTION_VECTOR_SHAPES.iter().enumerate() {
            assert!(
                !EXCEPTION_VECTOR_SHAPES[..index]
                    .iter()
                    .any(|&(seen, _)| seen == vector),
                "vector {vector} declared twice"
            );
        }
    }

    #[test]
    fn the_saved_cs_decodes_the_originating_ring() {
        // The kernel CS the GDT hands out is RPL 0; a ring-3 frame's CS
        // carries RPL 3.
        assert!(!cs_is_user(0x08));
        assert!(cs_is_user(0x2B));
        assert!(cs_is_user(0x1B));
    }
}
