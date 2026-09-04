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
//! A ring-3 exception a *user instruction raised* is the running task's
//! fault, not the CPU's, and costs only that task: the tail hands it to
//! the installed [`crate::fault::UserFaultTerminateFn`], which records the
//! crash exit, reclaims the task, and suspends it with an exit action — so
//! the CPU carries on running other work instead of parking for one
//! process's `ud2`.
//!
//! Not every vector is chargeable that way, and the table below declares
//! which, per vector. `#NMI` is an external interrupt rather than an
//! exception, `#DF` an abort whose saved state Intel documents as
//! unreliable, and `#MC` an imprecise machine-level abort: none is the
//! interrupted task's doing, and the first two are delivered on a shared
//! per-CPU IST stack a reschedule must never abandon. Those, the reserved
//! vectors, and every same-privilege (kernel) exception are the kernel's
//! own and keep the fatal report, whose syndrome still records the
//! originating ring honestly ([`crate::fault::syndrome_from_user`]).

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

/// Who a vector's exception belongs to when it is taken from ring 3.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum Origin {
    /// A fault or trap the executing instruction itself raised (Intel SDM
    /// Vol 3A Table 6-1), so a ring-3 delivery is the running task's own
    /// and kills only that task.
    Task,
    /// Not attributable to the interrupted instruction — an external
    /// interrupt (`#NMI`), a machine-level abort (`#DF`, `#MC`), or an
    /// architecturally undefined vector. A ring-3 delivery is still the
    /// kernel's own failure and takes the fatal report; charging a task
    /// for one would be a fabrication.
    Machine,
}

/// The [`Origin`] declared for `vector`, or [`Origin::Machine`] for a
/// vector this module does not own — an unowned vector cannot be charged
/// to a task (fail closed).
///
/// Its caller is the freestanding fatal tail, so it exists there and under
/// the host tests that pin the classification, and nowhere else.
#[cfg(any(test, all(target_arch = "x86_64", target_os = "none")))]
#[must_use]
const fn origin_of(vector: u8) -> Origin {
    let mut index = 0;
    while index < EXCEPTION_VECTOR_SHAPES.len() {
        let (owned, _, origin) = EXCEPTION_VECTOR_SHAPES[index];
        if owned == vector {
            return origin;
        }
        index += 1;
    }
    Origin::Machine
}

/// Terminate the offending task for a **ring-3** exception its own
/// instruction raised and keep the CPU alive; report and park only for a
/// kernel-mode exception, a machine-level vector, or one that cannot be
/// attributed to a running task.
///
/// This is the one fatal tail every exception stub funnels into. A user
/// task's own bad instruction (`ud2`, a privileged instruction, a
/// misaligned access under `#AC`) must cost only that task: parking the
/// whole CPU here — with interrupts masked, forever — turns a one-task
/// fault into an unprivileged, machine-wide denial of service. For a
/// ring-3 delivery on an [`Origin::Task`] vector it hands the running task
/// to the installed [`crate::fault::UserFaultTerminateFn`], which records
/// the crash exit, reclaims the task, and suspends it with an exit action —
/// that suspension switches to the dispatcher and never returns here. The
/// terminator returns only when the exception cannot be attributed to a
/// running task, which — like a kernel-mode fault — is genuinely
/// unrecoverable and falls through below (so a missing install can only
/// fail closed).
///
/// The unrecoverable tail packs the vector, the hardware error code and the
/// originating privilege level into the neutral syndrome word
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
///
/// # Safety
///
/// `saved` must be the live 15-GPR [`crate::interrupts::SavedRegs`] block
/// the stub persisted on this stack; it is only read here.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
unsafe fn fatal_exception(
    vector: u8,
    error_code: u64,
    rip: u64,
    cs: u64,
    saved: *const crate::interrupts::SavedRegs,
    user_rsp: u64,
) -> ! {
    let from_user = cs_is_user(cs);
    if from_user && matches!(origin_of(vector), Origin::Task) {
        if let Some(terminate) = crate::fault::user_fault_terminator() {
            // On success the terminator suspends the killed task and never
            // returns here (control switches to the dispatcher); a return
            // means the exception could not be attributed to a running
            // task, so fall through to the unrecoverable path below.
            // SAFETY: the caller's contract gives us the live saved block,
            // and `from_user` proved the exception came from ring 3, so the
            // helper's GS bracket is balanced.
            let _ = unsafe {
                crate::fault::with_ring3_context(saved, rip, user_rsp, |regs| terminate(rip, regs))
            };
        }
    }
    let syndrome = crate::fault::exception_syndrome(vector, error_code, from_user);
    if let Some(handler) = crate::fault::fault_handler() {
        handler(syndrome, 0, rip);
    }
    crate::reset::park_cpu()
}

/// Rust dispatcher every generated exception stub calls.
///
/// The arguments are the stub's marshalling of `(vector immediate,
/// hardware error code or a synthetic zero, faulting `rip`, saved `cs`,
/// the saved GPR block, the interrupted `rsp`)`. Diverges, so the stubs
/// need no epilogue.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[no_mangle]
extern "C" fn tairix_arch_x86_64_exception_dispatch(
    vector: u64,
    error_code: u64,
    rip: u64,
    cs: u64,
    saved: *const crate::interrupts::SavedRegs,
    user_rsp: u64,
) -> ! {
    // The stub passes its own `const` vector, which the generator bounds
    // to `0..=31`; mask rather than widen the report's vector field on a
    // value that cannot occur.
    #[allow(clippy::cast_possible_truncation)]
    // SAFETY-INVARIANT: masked to 8 bits, so the narrowing is lossless.
    let vector = (vector & 0xFF) as u8;
    // SAFETY: `saved` is the stub's own `%rsp` at the base of the 15-GPR
    // block it just pushed on this stack, which outlives this diverging
    // call.
    unsafe { fatal_exception(vector, error_code, rip, cs, saved, user_rsp) }
}

/// Declare every exception vector's stub and the tables the installer and
/// the fatal tail walk, so a vector is written once with all of its facts
/// and they cannot drift apart.
///
/// Each row is `<vector> => <stub name>, <origin>` with an optional
/// trailing `error_code` marking a vector the CPU pushes a hardware error
/// code for. `<origin>` is `task` or `machine` ([`Origin`]); an
/// unrecognised word fails to expand rather than defaulting, so a new
/// vector cannot be added without deciding whose fault it is.
macro_rules! exception_vectors {
    ( $( $vector:literal => $name:ident, $origin:ident $( , $err:ident )? );* $(;)? ) => {
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

        /// Every owned vector, whether the CPU pushes a hardware error
        /// code for it, and whose fault a ring-3 delivery is. Compiled on
        /// every configuration, so the table's coverage is both build-time
        /// asserted and host-unit-tested without the freestanding stubs.
        const EXCEPTION_VECTOR_SHAPES: &[(u8, bool, Origin)] = &[
            $((
                $vector,
                exception_vectors!(@has_err $( $err )?),
                exception_vectors!(@origin $origin),
            )),*
        ];
    };
    (@has_err error_code) => { true };
    (@has_err) => { false };
    (@origin task) => { Origin::Task };
    (@origin machine) => { Origin::Machine };
}

// Intel SDM Vol 3A Table 6-1. Vector 14 (`#PF`) is absent: it keeps the
// resumable entry in `crate::fault`. Vector 9 has not been generated since
// the i386 and the reserved vectors are architecturally undefined, so
// neither is a task's doing.
exception_vectors! {
    0 => isr_divide_error, task;
    1 => isr_debug, task;
    2 => isr_nmi, machine;
    3 => isr_breakpoint, task;
    4 => isr_overflow, task;
    5 => isr_bound_range, task;
    6 => isr_invalid_opcode, task;
    7 => isr_device_not_available, task;
    8 => isr_double_fault, machine, error_code;
    9 => isr_coprocessor_segment_overrun, machine;
    10 => isr_invalid_tss, task, error_code;
    11 => isr_segment_not_present, task, error_code;
    12 => isr_stack_segment_fault, task, error_code;
    13 => isr_general_protection, task, error_code;
    15 => isr_reserved_15, machine;
    16 => isr_fpu_error, task;
    17 => isr_alignment_check, task, error_code;
    18 => isr_machine_check, machine;
    19 => isr_simd_fp, task;
    20 => isr_virtualisation, task;
    21 => isr_control_protection, task, error_code;
    22 => isr_reserved_22, machine;
    23 => isr_reserved_23, machine;
    24 => isr_reserved_24, machine;
    25 => isr_reserved_25, machine;
    26 => isr_reserved_26, machine;
    27 => isr_reserved_27, machine;
    28 => isr_reserved_28, machine;
    29 => isr_reserved_29, machine;
    30 => isr_reserved_30, machine;
    31 => isr_reserved_31, machine;
}

/// A vector outside the exception range, or the `#PF` this module must
/// not claim, is a typo in the table above — catch it at build time rather
/// than by installing a gate over the resumable page-fault entry or over
/// a device's interrupt vector.
const _: () = {
    let mut index = 0;
    while index < EXCEPTION_VECTOR_SHAPES.len() {
        let (vector, _, _) = EXCEPTION_VECTOR_SHAPES[index];
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

    /// The vectors whose ring-3 delivery is *not* the interrupted task's
    /// doing: `#NMI` (an external interrupt, on an IST stack), `#DF` (an
    /// abort with unreliable saved state, on an IST stack), `#MC` (an
    /// imprecise machine-level abort), vector 9 (unused since the i386),
    /// and the architecturally reserved vectors.
    const MACHINE_VECTORS: &[u8] = &[2, 8, 9, 15, 18, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31];

    #[test]
    fn every_exception_vector_but_the_page_fault_has_a_stub() {
        for vector in 0..=LAST_EXCEPTION_VECTOR {
            let present = EXCEPTION_VECTOR_SHAPES.iter().any(|&(v, ..)| v == vector);
            assert_eq!(
                present,
                vector != PAGE_FAULT_VECTOR,
                "vector {vector} coverage"
            );
        }
    }

    #[test]
    fn the_error_code_marker_matches_the_intel_sdm() {
        for &(vector, has_error_code, _) in EXCEPTION_VECTOR_SHAPES {
            assert_eq!(
                has_error_code,
                ERROR_CODE_VECTORS.contains(&vector),
                "vector {vector} error-code shape"
            );
        }
    }

    #[test]
    fn no_vector_is_declared_twice() {
        for (index, &(vector, ..)) in EXCEPTION_VECTOR_SHAPES.iter().enumerate() {
            assert!(
                !EXCEPTION_VECTOR_SHAPES[..index]
                    .iter()
                    .any(|&(seen, ..)| seen == vector),
                "vector {vector} declared twice"
            );
        }
    }

    #[test]
    fn only_instruction_raised_vectors_are_charged_to_a_task() {
        for &(vector, _, origin) in EXCEPTION_VECTOR_SHAPES {
            let expected = if MACHINE_VECTORS.contains(&vector) {
                Origin::Machine
            } else {
                Origin::Task
            };
            assert_eq!(origin, expected, "vector {vector} origin");
            assert_eq!(origin_of(vector), expected, "vector {vector} lookup");
        }
    }

    /// An IST-routed vector must never terminate a task: the terminator
    /// reschedules, and abandoning a shared per-CPU IST stack
    /// mid-suspension would corrupt the next delivery on that stack. Read
    /// from the shared IST mapping, so adding an IST-routed vector cannot
    /// leave this invariant behind.
    #[test]
    fn no_ist_routed_vector_is_ever_charged_to_a_task() {
        let mut ist_routed = 0;
        for vector in 0..=LAST_EXCEPTION_VECTOR {
            if crate::percpu::ist_for_vector(vector) != 0 {
                ist_routed += 1;
                assert_eq!(origin_of(vector), Origin::Machine, "vector {vector} IST");
            }
        }
        // The mapping is expected to route some vector, so a future
        // refactor that empties it cannot make this test vacuous.
        assert!(ist_routed > 0);
    }

    /// A vector this module does not own — the resumable `#PF`, or
    /// anything in the interrupt range — cannot be charged to a task.
    #[test]
    fn an_unowned_vector_is_never_charged_to_a_task() {
        assert_eq!(origin_of(PAGE_FAULT_VECTOR), Origin::Machine);
        assert_eq!(origin_of(LAST_EXCEPTION_VECTOR + 1), Origin::Machine);
        assert_eq!(origin_of(u8::MAX), Origin::Machine);
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
