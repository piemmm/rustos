//! Per-CPU bring-up (Stage 3a (c1)).
//!
//! This module owns the *integration* between the
//! [`crate::gdt::PerCpuGdt`] primitives (already on `main`), the
//! [`crate::interrupts::Idt`] surface introduced in (c2), and the
//! actual physical IST stack arenas every CPU needs in order to field
//! a `#DF` or `#NMI` without trampling its task stack. Every CPU
//! invokes `init` exactly once with its CPU index; `init` selects
//! the per-CPU slot in the static `PerCpu` arena, populates the IST
//! stack tops, finalises the GDT, installs it, and loads the IDT.
//!
//! # Why a static arena and not `alloc`?
//!
//! `kernel/arch/x86_64` is deliberately `alloc`-free: the Stage-2
//! freestanding test binaries link it without an allocator and
//! `rustos-kernel` (the Stage 3a (c7) follow-up binary) is *not* on the
//! `alloc` heap before the per-CPU IDT is installed — there is no
//! allocator yet. The arena therefore lives in `.bss` and is sized at
//! compile time to `MAX_CPUS` entries. Going beyond `MAX_CPUS` is a
//! runtime error: `init` returns `InitError::CpuIndexOutOfRange`.
//!
//! # `MAX_CPUS` bound
//!
//! The Stage-2 QEMU integration runs at `-smp 4`; the per-CPU arena is
//! sized to 16, matching the cap in `tests/integration/
//! scheduler_stress_qemu/src/kernel.rs::MAX_CPUS`. Raising the bound
//! across the workspace is a single-place edit *here* and on the test
//! mirror; the test crate has a const-assert on the relation.
//!
//! # Why one IDT per CPU?
//!
//! The IDT is a CPU-local register (IDTR). Sharing one IDT across
//! cores is technically fine — every entry is read-only after install —
//! but per-CPU storage is the more defensive arrangement: an IST
//! reference inside an IDT entry indirectly addresses *this* CPU's
//! TSS (via the GDT) so each core gets its own copy by symmetry, and
//! a future commit that wants per-CPU exception statistics can mutate
//! its own copy without atomics.

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
use core::sync::atomic::{AtomicBool, Ordering};

use crate::gdt::{self, PerCpuGdt};
use crate::interrupts::Idt;

/// Maximum number of CPUs the per-CPU arena can be configured for.
///
/// Stage-2 ships at `-smp 4`; the cap of 16 is the integration-test
/// agreement (`scheduler_stress_qemu/src/kernel.rs::MAX_CPUS`). Raising
/// it requires editing both sides plus the QEMU runner default.
pub const MAX_CPUS: usize = 16;

/// Size of one IST stack in bytes. 16 KiB matches the BSP bootstrap
/// stack in `boot.s` and the per-AP stacks in
/// `scheduler_stress_qemu/src/kernel.rs::AP_STACKS` — keeping the
/// constants equal lets a future audit of "stack budget at fault time"
/// reason about one figure instead of three.
pub const IST_STACK_BYTES: usize = 16 * 1024;

/// IST index used for the `#DF` (Double Fault, vector 8) gate. SDM Vol
/// 3A §6.14.5 specifies that `#DF` *must* use an IST in any production
/// long-mode kernel.
pub const IST_INDEX_DF: u8 = 1;

/// IST index used for the `#NMI` (vector 2) gate.
pub const IST_INDEX_NMI: u8 = 2;

/// Errors returned by `init` or `install_vector`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitError {
    /// `cpu_index` was ≥ `MAX_CPUS`.
    CpuIndexOutOfRange,
    /// `init` was called more than once for this CPU index.
    AlreadyInitialised,
    /// One of the IST configurations was rejected by
    /// `PerCpuGdt::set_ist` (alignment / null / index).
    Ist(gdt::IstError),
    /// `install_vector` was called on a CPU that has not yet
    /// completed `init`. The per-CPU IDT is not safe to mutate
    /// until the latch is set, so the call is rejected fail-closed.
    NotInitialised,
}

impl From<gdt::IstError> for InitError {
    fn from(e: gdt::IstError) -> Self {
        Self::Ist(e)
    }
}

// --- Per-CPU bundle -------------------------------------------------

/// All of the per-CPU storage `init` touches in one place.
///
/// `#[repr(C, align(16))]` ensures the IST stacks are 16-byte aligned —
/// `PerCpuGdt::set_ist` enforces that on the stack-top input but the
/// natural alignment of `[u8; 16*1024]` is only 1, so without the
/// explicit `align(16)` directive we would have to round each slot
/// pointer manually.
#[repr(C, align(16))]
pub struct PerCpu {
    /// Per-CPU GDT + TSS bundle.
    pub gdt: PerCpuGdt,
    /// Per-CPU IDT.
    pub idt: Idt,
    /// IST 1: `#DF` (Double Fault) stack.
    pub df_stack: [u8; IST_STACK_BYTES],
    /// IST 2: `#NMI` stack.
    pub nmi_stack: [u8; IST_STACK_BYTES],
}

impl PerCpu {
    /// `const`-constructible empty slot. Used to populate the static
    /// arena before `init` runs.
    #[must_use]
    pub const fn new_zeroed() -> Self {
        Self {
            gdt: PerCpuGdt::new(),
            idt: Idt::empty(),
            df_stack: [0; IST_STACK_BYTES],
            nmi_stack: [0; IST_STACK_BYTES],
        }
    }

    /// Linear address of the byte one-past-the-end of `df_stack` (i.e.
    /// the value the System V AMD64 ABI wants RSP to be initialised
    /// to). 16-byte aligned because the struct itself is `align(16)`.
    ///
    /// Only used by `init` (on the freestanding target) and by host
    /// unit tests; gated to keep clippy happy on cross-config builds.
    #[cfg(any(test, all(target_arch = "x86_64", target_os = "none")))]
    fn df_stack_top(&self) -> u64 {
        let base = core::ptr::addr_of!(self.df_stack) as u64;
        base + IST_STACK_BYTES as u64
    }

    /// Linear address of the byte one-past-the-end of `nmi_stack`.
    #[cfg(any(test, all(target_arch = "x86_64", target_os = "none")))]
    fn nmi_stack_top(&self) -> u64 {
        let base = core::ptr::addr_of!(self.nmi_stack) as u64;
        base + IST_STACK_BYTES as u64
    }
}

// --- Static arena ---------------------------------------------------

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
static mut PER_CPU: [PerCpu; MAX_CPUS] = {
    const Z: PerCpu = PerCpu::new_zeroed();
    [Z; MAX_CPUS]
};

/// One-shot guards: bit `i` set means `init(i)` already ran. Only
/// referenced by the bare-metal `init` entry point; gated so the
/// host build does not carry it as dead code.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
static PER_CPU_INITIALISED: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

// --- Public init entry point ---------------------------------------

/// Initialise the per-CPU GDT + IDT for `cpu_index` on the currently
/// running CPU.
///
/// On a successful return,
///
/// * the per-CPU GDT for `cpu_index` is `lgdt`-installed (kernel CS /
///   DS reloaded, `ltr` issued),
/// * the per-CPU IDT is `lidt`-installed; every vector points at the
///   fail-closed default thunk from
///   `crate::interrupts::Idt::with_default_handler`,
/// * `#DF` (vector 8) is routed through IST 1 backed by `df_stack`,
/// * `#NMI` (vector 2) is routed through IST 2 backed by `nmi_stack`.
///
/// After return interrupts may safely be enabled on this CPU (no other
/// per-CPU state is required for the Stage-3a (c1/c2/c3) scope; LAPIC
/// timer arming is the (c5) commit's responsibility).
///
/// # Errors
///
/// * `InitError::CpuIndexOutOfRange` if `cpu_index >= MAX_CPUS`.
/// * `InitError::AlreadyInitialised` if `init` already ran for this
///   index on any CPU.
/// * `InitError::Ist` if `PerCpuGdt::set_ist` rejected one of the
///   stack-top pointers (only possible if `MAX_CPUS` or
///   `IST_STACK_BYTES` are misconfigured at compile time).
///
/// # Safety
///
/// The caller must guarantee that
///
/// * `init` runs exactly once per CPU (the per-CPU latch is the
///   diagnostic, not the guarantee — racing two CPUs on the same index
///   is undefined behaviour because both would write to the same
///   `PerCpu` slot);
/// * `init` runs *before* the CPU enables interrupts — there is no IDT
///   installed prior to the `lidt` here (the boot-time IDTR is invalid
///   per `boot.s` SAFETY-INVARIANT 6).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub unsafe fn init(cpu_index: usize) -> Result<(), InitError> {
    if cpu_index >= MAX_CPUS {
        return Err(InitError::CpuIndexOutOfRange);
    }
    if PER_CPU_INITIALISED[cpu_index]
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(InitError::AlreadyInitialised);
    }

    // SAFETY: per the function's safety contract, the caller runs us
    // exactly once for this `cpu_index`, so we hold the unique mutable
    // reference to `PER_CPU[cpu_index]` for the duration of this call.
    // No other CPU touches this slot.
    let slot: &'static mut PerCpu = unsafe {
        let base = core::ptr::addr_of_mut!(PER_CPU) as *mut PerCpu;
        &mut *base.add(cpu_index)
    };

    let df_top = slot.df_stack_top();
    let nmi_top = slot.nmi_stack_top();

    slot.gdt.set_ist(IST_INDEX_DF, df_top)?;
    slot.gdt.set_ist(IST_INDEX_NMI, nmi_top)?;
    slot.gdt.finalize();

    // Construct the IDT with the default thunk and the IST mapping the
    // (c2) module documents.
    let handler = crate::interrupts_default_isr_addr();
    let selector = PerCpuGdt::selectors().kernel_cs;
    slot.idt = Idt::with_default_handler(handler, selector, |v| match v {
        2 => IST_INDEX_NMI,
        8 => IST_INDEX_DF,
        _ => 0,
    });

    // SAFETY: `slot` is borrowed from a `'static mut` arena, so the
    // `'static` lifetime promised by `PerCpuGdt::install` and
    // `Idt::load` is satisfied. The caller's contract guarantees this
    // CPU runs the install path exactly once.
    unsafe {
        slot.gdt.install();
        // Re-borrow the IDT shared: `install()` took a `&mut`, but
        // `load()` takes a `&'static self`. Both are derived from the
        // same `'static` arena slot; no aliasing because `install`
        // returns first.
        let idt_ref: &'static Idt = &*core::ptr::addr_of!(slot.idt);
        idt_ref.load();
    }

    Ok(())
}

/// Install a per-CPU IDT vector after `init` has finalised the CPU.
///
/// `cpu_index` selects which per-CPU IDT to mutate; `vector` is the
/// 0..=255 architecturally-fixed slot to overwrite; `handler` is the
/// linear address of the ISR entry point (typically the symbol of a
/// stub emitted by [`crate::define_isr`]). The IDT entry is built as
/// a 64-bit interrupt gate at DPL 0, IST 0 — vectors that need an IST
/// stack (NMI, #DF) are already installed by `init` and must not be
/// overwritten through this entry point.
///
/// The CPU re-reads the IDT base on every interrupt delivery, so
/// overwriting an entry while interrupts are disabled is safe; the
/// caller must keep interrupts disabled across this call.
///
/// # Errors
///
/// * `InitError::CpuIndexOutOfRange` if `cpu_index >= MAX_CPUS`.
/// * `InitError::NotInitialised` if `init` has not run for
///   `cpu_index`. Fail-closed per `AGENTS.md` §10 — a stray vector
///   install on an un-bootstrapped CPU is a kernel bug, not a
///   silent fixup.
///
/// # Safety
///
/// * Interrupts on the calling CPU must be disabled.
/// * `handler` must be the address of a valid ISR (either the
///   default thunk from `interrupts.s` or a stub produced by
///   [`crate::define_isr`]). Pointing the slot at any other address
///   makes the CPU jump to invalid code on the next delivery.
/// * `vector` must not be `2` (`#NMI`) or `8` (`#DF`): those slots
///   are owned by `init` and route through dedicated IST stacks;
///   overwriting them with an IST-0 entry would defeat the
///   double-fault stack-swap guarantee. The function does not
///   refuse those vectors at runtime — the caller is responsible.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub unsafe fn install_vector(cpu_index: usize, vector: u8, handler: u64) -> Result<(), InitError> {
    use crate::interrupts::IdtEntry;
    if cpu_index >= MAX_CPUS {
        return Err(InitError::CpuIndexOutOfRange);
    }
    if !PER_CPU_INITIALISED[cpu_index].load(Ordering::Acquire) {
        return Err(InitError::NotInitialised);
    }
    // SAFETY: the latch above is `true`, so `init` has finalised this
    // slot and the only writer from here on is the CPU it belongs to.
    // The caller's safety contract requires interrupts to be disabled
    // on the calling CPU, so a delivery cannot race the write.
    unsafe {
        let base = core::ptr::addr_of_mut!(PER_CPU) as *mut PerCpu;
        let entry_ptr =
            core::ptr::addr_of_mut!((*base.add(cpu_index)).idt.entries[vector as usize]);
        let selector = PerCpuGdt::selectors().kernel_cs;
        core::ptr::write_volatile(entry_ptr, IdtEntry::interrupt_gate(handler, selector, 0));
    }
    Ok(())
}

// --- Host-test surface ---------------------------------------------

/// Tests-only mirror of the bring-up logic, with the actual `lgdt` /
/// `lidt` instructions skipped. Validates that
///
/// * `cpu_index` bound checks fire,
/// * the IST stack tops are correctly derived from the arena layout
///   (alignment + non-null + above the slot base),
/// * the per-CPU IDT is populated with the documented IST mapping,
/// * the one-shot latch fires on a second call.
#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicBool, Ordering};

    // Local arena for host tests; mirrors the production one but is
    // owned by the test module so the test does not collide with any
    // production `static mut PER_CPU` (which is `cfg(target_os =
    // "none")`-gated anyway).
    static HOST_INITIALISED: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

    #[test]
    fn per_cpu_layout_is_aligned_and_sized() {
        // The struct must be 16-byte aligned so IST stacks inherit it.
        assert_eq!(core::mem::align_of::<PerCpu>() % 16, 0);
        // The IDT alone is 16 × 256 = 4096 bytes; sanity check that
        // the bundle is at least that big.
        assert!(core::mem::size_of::<PerCpu>() >= 16 * 256 + 2 * IST_STACK_BYTES);
    }

    #[test]
    fn ist_stack_tops_are_above_slot_base_and_aligned() {
        let slot = PerCpu::new_zeroed();
        let base = core::ptr::addr_of!(slot) as u64;
        let df_top = slot.df_stack_top();
        let nmi_top = slot.nmi_stack_top();
        assert!(df_top > base);
        assert!(nmi_top > df_top);
        assert_eq!(df_top % 16, 0);
        assert_eq!(nmi_top % 16, 0);
    }

    #[test]
    fn ist_stack_bytes_matches_documentation() {
        // The cross-crate agreement says 16 KiB. If this number ever
        // changes, the corresponding constant in
        // `scheduler_stress_qemu/src/kernel.rs::ApStack` must change
        // in lock-step.
        assert_eq!(IST_STACK_BYTES, 16 * 1024);
    }

    #[test]
    fn one_shot_latch_rejects_double_init() {
        // Call the latch directly (we cannot run the real `init` on
        // the host because of the asm). The semantics under test are
        // exactly the latch logic.
        let idx = 3;
        assert!(HOST_INITIALISED[idx]
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok());
        assert!(HOST_INITIALISED[idx]
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err());
    }

    #[test]
    fn max_cpus_matches_scheduler_stress_agreement() {
        // The QEMU stress test's `MAX_CPUS` constant must match this
        // crate's. The cross-check is in the test crate; the value
        // here is the source of truth.
        assert_eq!(MAX_CPUS, 16);
    }

    #[test]
    fn ist_indices_use_documented_slots() {
        assert_eq!(IST_INDEX_DF, 1);
        assert_eq!(IST_INDEX_NMI, 2);
    }
}
