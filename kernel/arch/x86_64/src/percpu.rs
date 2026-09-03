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
//! # Why caller-provided storage and not a fixed `.bss` arena?
//!
//! `kernel/arch/x86_64` is deliberately `alloc`-free: the freestanding
//! QEMU test binaries link it without an allocator and `tairix-kernel`
//! is *not* on the `alloc` heap before the per-CPU IDT is installed —
//! there is no allocator yet. A fixed `static mut PER_CPU: [PerCpu;
//! MAX_CPUS]` would therefore size the per-CPU arena to a hand-picked
//! compile-time constant that a larger machine outgrows and a smaller
//! one wastes — exactly the "no fixed capacity ceiling" defect.
//!
//! Instead the per-CPU arena is a caller-owned [`PerCpuStorage`]: the
//! constructing boot path sizes `N` for the machine's-discovered
//! logical-CPU count, places it in a `static` (allocator-free bins) or a
//! leaked allocation, and publishes it through
//! [`PerCpuStorage::register`] before the first `init`. The per-CPU
//! entry points then index the registered slices, bounds-checked against
//! the published length; before registration — or for an out-of-range
//! index — they fail closed with [`InitError::CpuIndexOutOfRange`]. This mirrors the aarch64
//! `smp::SecondaryStackPool` / riscv64 `smp::SecondaryStackPool`
//! caller-sized secondary-bring-up pools and the crate's own
//! `crate::kernel_arch::X86_64ArchStorage`.
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

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

use crate::gdt::{self, PerCpuGdt};
use crate::interrupts::Idt;

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

/// The IST index vector `vector`'s gate must carry, or `0` for a gate
/// that runs on the interrupted stack.
///
/// `#DF` and `#NMI` are the two vectors Intel SDM Vol 3A §6.14.5 says a
/// production long-mode kernel must give a dedicated stack: `#DF` because
/// it is precisely the exception raised when the current stack cannot be
/// used, `#NMI` because it can arrive on any stack at any time. Every
/// other vector is fine on the interrupted stack.
///
/// One definition, read by both `init` (which populates the whole table)
/// and `install_vector` (which overwrites individual slots later), so an
/// overwrite cannot silently drop a gate's IST and defeat the stack swap.
#[must_use]
pub const fn ist_for_vector(vector: u8) -> u8 {
    match vector {
        2 => IST_INDEX_NMI,
        8 => IST_INDEX_DF,
        _ => 0,
    }
}

/// Errors returned by `init` or `install_vector`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitError {
    /// `cpu_index` was outside the registered [`PerCpuStorage`] (or no
    /// storage is registered yet — fail closed).
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
    /// A kernel `RSP0` (the ring-0 stack the CPU loads on a `syscall`
    /// from ring 3) was rejected: null, not 16-byte aligned,
    /// non-canonical, or in the user half of the address space
    /// (stack-pivot / CVE-2019-1125 class).
    InvalidKernelStackPointer,
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

// --- Caller-provided per-CPU storage --------------------------------

/// Published base of the registered [`PerCpuStorage::cpus`] array
/// (`null` until a storage is registered, so every per-CPU entry point
/// fails closed before registration).
static PER_CPU_BASE: AtomicPtr<PerCpu> = AtomicPtr::new(core::ptr::null_mut());

/// Published base of the registered [`PerCpuStorage::initialised`]
/// one-shot latch array (`null` until a storage is registered). Latch
/// `i` is set the first time `init(i)` runs on any CPU.
static PER_CPU_INIT_BASE: AtomicPtr<AtomicBool> = AtomicPtr::new(core::ptr::null_mut());

/// Number of logical-CPU slots the registered storage covers (`0` until
/// a storage is registered, so an unregistered system fails closed —
/// every index is out of range).
static PER_CPU_LEN: AtomicUsize = AtomicUsize::new(0);

/// Set-once guard so a second [`PerCpuStorage::register`] is refused
/// rather than silently re-pointing the live per-CPU slices.
static PER_CPU_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Failure mode of [`PerCpuStorage::register`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum PerCpuStorageError {
    /// Storage was already registered; the slot is set-once per boot
    /// (no silent re-pointing of the live arena).
    AlreadyRegistered,
}

/// Caller-owned, `&'static` per-CPU GDT/IDT/IST arena, sized by the
/// constructing caller for its machine (the per-CPU
/// arena is derived from the-discovered logical-CPU count, never a
/// fixed `const` ceiling baked into the arch crate).
///
/// The const parameter `N` is the number of logical CPUs the caller
/// sizes for: a single-CPU boot path uses `PerCpuStorage<1>`, and a
/// multi-core boot path sizes `N` from the ACPI MADT processor count.
/// The arch crate stays allocator-free (watch-out — no
/// `alloc` in a bare-metal arch crate, which would force a heap into the
/// freestanding QEMU bins), so the caller provides the storage as a
/// `static` (allocator-free bins) or a leaked allocation and publishes it
/// through [`PerCpuStorage::register`] before the first `init`.
#[repr(C, align(16))]
pub struct PerCpuStorage<const N: usize> {
    /// Per-CPU GDT/IDT/IST bundles, one slot per logical CPU. The
    /// `UnsafeCell` is load-bearing: `init` (and the AP-bring-up /
    /// `syscall` asm) mutate a slot through the published base while
    /// the storage is only borrowed `&'static` (shared), so the
    /// interior mutability is what makes those writes sound *and* keeps
    /// the `static` in writable memory rather than read-only `.rodata`.
    cpus: UnsafeCell<[PerCpu; N]>,
    /// One-shot `init` latches, one per slot (`false` until `init` runs).
    initialised: [AtomicBool; N],
}

// SAFETY: the `UnsafeCell<[PerCpu; N]>` is mutated only through the
// published base, and the bare-metal contract gives each CPU exclusive
// ownership of its own slot (`init` runs once per `cpu_index`, on that
// CPU); the host tests touch only the `AtomicBool` latches. No slot is
// shared mutably across threads/CPUs, so the storage is `Sync`.
unsafe impl<const N: usize> Sync for PerCpuStorage<N> {}

impl<const N: usize> PerCpuStorage<N> {
    /// A zeroed arena of `N` per-CPU bundles with every `init` latch
    /// clear. `const` so the allocator-free bins can place it in a
    /// `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cpus: UnsafeCell::new([const { PerCpu::new_zeroed() }; N]),
            initialised: [const { AtomicBool::new(false) }; N],
        }
    }

    /// Publish this arena to the per-CPU entry points, then return the
    /// covered CPU count `N`. Must be called on the boot CPU, exactly
    /// once, before any `init`.
    ///
    /// # Errors
    ///
    /// [`PerCpuStorageError::AlreadyRegistered`] on the second publish
    /// (set-once per boot — never silently re-points the live arena).
    pub fn register(&'static self) -> Result<usize, PerCpuStorageError> {
        if PER_CPU_REGISTERED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(PerCpuStorageError::AlreadyRegistered);
        }
        PER_CPU_BASE.store(self.cpus.get().cast::<PerCpu>(), Ordering::Release);
        PER_CPU_INIT_BASE.store(self.initialised.as_ptr().cast_mut(), Ordering::Release);
        PER_CPU_LEN.store(N, Ordering::Release);
        Ok(N)
    }
}

impl<const N: usize> Default for PerCpuStorage<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Number of logical-CPU slots the registered [`PerCpuStorage`] covers
/// (`0` until a storage is registered). Diagnostic observer.
#[must_use]
pub fn registered_cpu_count() -> usize {
    PER_CPU_LEN.load(Ordering::Acquire)
}

/// Raw pointer to the registered per-CPU slot for `cpu_index`, or
/// `None` if `cpu_index` is out of range or no storage is registered
/// yet (fail closed).
#[cfg(any(test, all(target_arch = "x86_64", target_os = "none")))]
fn per_cpu_ptr(cpu_index: usize) -> Option<*mut PerCpu> {
    if cpu_index >= PER_CPU_LEN.load(Ordering::Acquire) {
        return None;
    }
    let base = PER_CPU_BASE.load(Ordering::Acquire);
    if base.is_null() {
        return None;
    }
    // SAFETY: a non-zero `PER_CPU_LEN` (checked above) is published in
    // the same `register` call that stores the non-null base from a
    // `&'static PerCpuStorage`'s `cpus` array of that length, and
    // `cpu_index < len`, so `base.add(cpu_index)` is in bounds.
    Some(unsafe { base.add(cpu_index) })
}

/// The `init` latch for `cpu_index`, or `None` if out of range / no
/// storage is registered (fail closed).
#[cfg(any(test, all(target_arch = "x86_64", target_os = "none")))]
fn per_cpu_initialised(cpu_index: usize) -> Option<&'static AtomicBool> {
    if cpu_index >= PER_CPU_LEN.load(Ordering::Acquire) {
        return None;
    }
    let base = PER_CPU_INIT_BASE.load(Ordering::Acquire);
    if base.is_null() {
        return None;
    }
    // SAFETY: as for [`per_cpu_ptr`] — the non-null `initialised` base
    // and the matching `PER_CPU_LEN` are published together, and
    // `cpu_index < len`; the referent lives for `'static`.
    Some(unsafe { &*base.add(cpu_index) })
}

#[cfg(test)]
fn reset_per_cpu_storage_for_tests() {
    PER_CPU_REGISTERED.store(false, Ordering::Release);
    PER_CPU_LEN.store(0, Ordering::Release);
    PER_CPU_BASE.store(core::ptr::null_mut(), Ordering::Release);
    PER_CPU_INIT_BASE.store(core::ptr::null_mut(), Ordering::Release);
}

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
/// * `InitError::CpuIndexOutOfRange` if `cpu_index` is outside the
///   registered [`PerCpuStorage`] (or no storage is registered).
/// * `InitError::AlreadyInitialised` if `init` already ran for this
///   index on any CPU.
/// * `InitError::Ist` if `PerCpuGdt::set_ist` rejected one of the
///   stack-top pointers (only possible if `IST_STACK_BYTES` is
///   misconfigured at compile time).
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
    // Fail closed before registration or for an out-of-range index: the registered storage's published
    // length is the only bound, not a baked-in `MAX_CPUS`.
    let slot_ptr = per_cpu_ptr(cpu_index).ok_or(InitError::CpuIndexOutOfRange)?;
    let latch = per_cpu_initialised(cpu_index).ok_or(InitError::CpuIndexOutOfRange)?;
    if latch
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(InitError::AlreadyInitialised);
    }

    // SAFETY: per the function's safety contract, the caller runs us
    // exactly once for this `cpu_index`, so we hold the unique mutable
    // reference to the registered `PerCpu` slot for the duration of this
    // call. No other CPU touches this slot. `slot_ptr` came from
    // `per_cpu_ptr`, which proved it points inside the `&'static`
    // registered storage.
    let slot: &'static mut PerCpu = unsafe { &mut *slot_ptr };

    let df_top = slot.df_stack_top();
    let nmi_top = slot.nmi_stack_top();

    slot.gdt.set_ist(IST_INDEX_DF, df_top)?;
    slot.gdt.set_ist(IST_INDEX_NMI, nmi_top)?;
    slot.gdt.finalize();

    // Construct the IDT with the default thunk and the IST mapping the
    // (c2) module documents.
    let handler = crate::interrupts_default_isr_addr();
    let selector = PerCpuGdt::selectors().kernel_cs;
    slot.idt = Idt::with_default_handler(handler, selector, ist_for_vector);

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
/// stub emitted by [`crate::define_isr`] or
/// [`crate::define_exception_isr`]). The IDT entry is built as a 64-bit
/// interrupt gate at DPL 0 whose IST index comes from the same
/// [`ist_for_vector`] mapping [`init`] used, so overwriting `#DF` or
/// `#NMI` keeps its dedicated stack rather than silently defeating the
/// stack swap.
///
/// The CPU re-reads the IDT base on every interrupt delivery, so
/// overwriting an entry while interrupts are disabled is safe; the
/// caller must keep interrupts disabled across this call.
///
/// # Errors
///
/// * `InitError::CpuIndexOutOfRange` if `cpu_index` is outside the
///   registered [`PerCpuStorage`] (or no storage is registered).
/// * `InitError::NotInitialised` if `init` has not run for
///   `cpu_index`. Fail-closed — a stray vector
///   install on an un-bootstrapped CPU is a kernel bug, not a
///   silent fixup.
///
/// # Safety
///
/// * Interrupts on the calling CPU must be disabled.
/// * `handler` must be the address of a valid ISR (either the
///   default thunk from `interrupts.s` or a stub produced by
///   [`crate::define_isr`] / [`crate::define_exception_isr`]). Pointing
///   the slot at any other address makes the CPU jump to invalid code on
///   the next delivery.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub unsafe fn install_vector(cpu_index: usize, vector: u8, handler: u64) -> Result<(), InitError> {
    use crate::interrupts::IdtEntry;
    let slot_ptr = per_cpu_ptr(cpu_index).ok_or(InitError::CpuIndexOutOfRange)?;
    let latch = per_cpu_initialised(cpu_index).ok_or(InitError::CpuIndexOutOfRange)?;
    if !latch.load(Ordering::Acquire) {
        return Err(InitError::NotInitialised);
    }
    // SAFETY: the latch above is `true`, so `init` has finalised this
    // slot and the only writer from here on is the CPU it belongs to.
    // The caller's safety contract requires interrupts to be disabled
    // on the calling CPU, so a delivery cannot race the write.
    // `slot_ptr` points inside the `&'static` registered storage.
    unsafe {
        let entry_ptr = core::ptr::addr_of_mut!((*slot_ptr).idt.entries[vector as usize]);
        let selector = PerCpuGdt::selectors().kernel_cs;
        core::ptr::write_volatile(
            entry_ptr,
            IdtEntry::interrupt_gate(handler, selector, ist_for_vector(vector)),
        );
    }
    Ok(())
}

/// Install the ring-0 stack the CPU loads into `RSP` on a ring-3 → ring-0
/// CPU exception or hardware interrupt — the `TSS.RSP0` the hardware reads
/// on every privilege-raising transition, distinct from the `syscall`
/// entry stack [`crate::syscall_entry::install_kernel_rsp0`] keeps in the
/// per-CPU TLS.
///
/// [`init`] does **not** set `RSP0`: the `syscall` path pivots onto its own
/// TLS stack via `swapgs`, so a kernel that only ever leaves ring 3 through
/// `syscall`/`sysret` never reads `TSS.RSP0`. The first time a ring-3 task
/// takes a CPU exception (`#PF`, `#GP`, …) or is preempted by a hardware
/// IRQ, the CPU loads `TSS.RSP0`; a zero (or unmapped) value makes the
/// interrupt-frame push fault and escalate to `#DF`. Installing a valid
/// `RSP0` makes a ring-3 trap *deliverable* to the kernel — a user fault
/// the kernel cannot field is a security gap, not a feature. This is the x86_64 counterpart of the single kernel trap
/// stack the riscv64/aarch64 ports already program.
///
/// The CPU re-reads `TSS.RSP0` from memory on every transition, so writing
/// it after the `ltr` [`init`] issued takes effect immediately; the caller
/// must keep interrupts disabled across the write so a delivery cannot
/// observe a half-written field.
///
/// # Errors
///
/// * [`InitError::CpuIndexOutOfRange`] if `cpu_index` is outside the
///   registered [`PerCpuStorage`] (or no storage is registered).
/// * [`InitError::NotInitialised`] if [`init`] has not finalised
///   `cpu_index` (fail-closed).
/// * [`InitError::InvalidKernelStackPointer`] if `rsp0` is null, not
///   16-byte aligned, non-canonical, or in the user half — the same
///   stack-pivot guard the syscall stack uses.
///
/// # Safety
///
/// * Interrupts on the calling CPU must be disabled.
/// * `cpu_index` must be this CPU's index (the one passed to [`init`]).
/// * `rsp0` must be one byte past the top of a kernel stack reserved for
///   ring-3 trap entry on this CPU and mapped in **every** address space
///   this CPU runs (including each user address space), so the
///   interrupt-frame push always lands on mapped memory.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub unsafe fn install_tss_rsp0(cpu_index: usize, rsp0: u64) -> Result<(), InitError> {
    let slot_ptr = per_cpu_ptr(cpu_index).ok_or(InitError::CpuIndexOutOfRange)?;
    let latch = per_cpu_initialised(cpu_index).ok_or(InitError::CpuIndexOutOfRange)?;
    if !latch.load(Ordering::Acquire) {
        return Err(InitError::NotInitialised);
    }
    // Strong stack-pivot guard, shared with the syscall-entry stack so the
    // two kernel-stack-top validators stay one definition. It is a superset of `set_privilege_stack`'s alignment/null
    // check, additionally rejecting non-canonical / user-half tops.
    crate::syscall_entry::validate_kernel_rsp0(rsp0)?;
    // SAFETY: the latch above is `true`, so `init` finalised this slot and
    // the only writer from here on is the CPU it belongs to; the caller's
    // contract keeps interrupts disabled so a delivery cannot race the
    // write. The in-memory `TSS.RSP0` the CPU re-reads on each transition
    // lives in this slot's GDT bundle. `slot_ptr` points inside the
    // `&'static` registered storage.
    unsafe {
        let gdt = &mut (*slot_ptr).gdt;
        gdt.set_privilege_stack(0, rsp0)?;
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

    #[test]
    fn only_the_double_fault_and_nmi_gates_carry_an_ist() {
        // Intel SDM Vol 3A §6.14.5: those two must run on a dedicated
        // stack. Every other vector runs on the interrupted stack, and the
        // mapping is shared by the table populator and the later per-vector
        // overwrites so an overwrite cannot silently drop the stack swap.
        assert_eq!(ist_for_vector(8), IST_INDEX_DF);
        assert_eq!(ist_for_vector(2), IST_INDEX_NMI);
        for vector in 0u8..=255 {
            if vector != 2 && vector != 8 {
                assert_eq!(ist_for_vector(vector), 0, "vector {vector}");
            }
        }
    }
    use core::sync::atomic::Ordering;

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
    fn ist_indices_use_documented_slots() {
        assert_eq!(IST_INDEX_DF, 1);
        assert_eq!(IST_INDEX_NMI, 2);
    }

    #[test]
    fn per_cpu_storage_registration_publishes_runtime_sized_slices() {
        // A caller-sized backing covers exactly its `N` slots (the
        // capacity is the-discovered CPU count, not a baked-in
        // `MAX_CPUS`); a second backing proves registration is set-once.
        // Declared first so they precede the statements that drive them.
        static STORAGE: PerCpuStorage<4> = PerCpuStorage::new();
        static STORAGE2: PerCpuStorage<2> = PerCpuStorage::new();

        reset_per_cpu_storage_for_tests();

        // Before any storage is registered every per-CPU accessor fails
        // closed (`None` / `0`) instead of dereferencing a null base.
        assert_eq!(registered_cpu_count(), 0);
        assert!(per_cpu_ptr(0).is_none());
        assert!(per_cpu_initialised(0).is_none());

        assert_eq!(STORAGE.register(), Ok(4));
        assert_eq!(registered_cpu_count(), 4);
        assert!(per_cpu_ptr(0).is_some());
        assert!(per_cpu_ptr(3).is_some());
        // An out-of-range index is rejected, not clamped.
        assert!(per_cpu_ptr(4).is_none());
        assert!(per_cpu_initialised(4).is_none());

        // The one-shot `init` latch round-trips through the published
        // slice (the bare-metal `init` flips the same slot).
        let latch = per_cpu_initialised(2).expect("registered latch");
        assert!(latch
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok());
        assert!(latch
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err());

        // Registration is set-once: a second backing is refused rather
        // than silently re-pointing the live slices.
        assert_eq!(
            STORAGE2.register(),
            Err(PerCpuStorageError::AlreadyRegistered)
        );

        reset_per_cpu_storage_for_tests();
    }
}
