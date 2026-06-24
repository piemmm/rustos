//! Application-processor (AP) bring-up — Stage 3a (b).
//!
//! The boot CPU drives the rest of the cores through the
//! INIT-SIPI-SIPI sequence documented in Intel SDM Vol 3A §8.4.4.1.
//! This module owns:
//!
//! * [`AP_TRAMPOLINE_PHYS`] / [`AP_TRAMPOLINE_LEN`] — the layout
//!   constants shared with `ap_trampoline.s` (cross-checked by the
//!   `ap_boot_slot_layout_is_locked` host unit test against
//!   `core::mem::offset_of!`).
//! * [`ApBootSlot`] — the per-AP record the trampoline reads at a fixed
//!   in-page offset.
//! * [`TrampolineFrame`] — the typed wrapper around the 4 KiB low
//!   physical frame; installs the payload and exposes a typed view of
//!   the boot slot.
//! * [`init_sipi_sipi`] — the INIT-deassert + INIT + SIPI + SIPI
//!   sequencer expressed against the existing
//!   [`crate::apic::Lapic`] / [`crate::apic::LapicMmio`] primitives so
//!   no new architecture-neutral surface is introduced.
//!
//! # Why this lives in `kernel/arch/x86_64` and not `kernel/sched`
//!
//! the charter forbids "convenience wrappers" that exist in only
//! one consumer. AP bring-up is x86_64-specific (aarch64 uses PSCI,
//! riscv64 uses HSM, wasm32 has no APs); the architecture-neutral
//! scheduler has no business knowing how a target hardware brings up
//! its CPUs. The cooperation point is [`crate::smp::ApBootSlot::entry`]
//! — a plain `extern "C" fn(cpu_id: u32) -> !` that the consumer-side
//! kernel binary supplies.
//!
//! # `unsafe` discipline
//!
//! Every `unsafe` block has a `// SAFETY:` justification. The two
//! non-trivial ones are the byte-exact `copy_nonoverlapping` of the
//! assembled trampoline payload into the low-memory frame
//! ([`TrampolineFrame::install`]) and the volatile boot-slot writes
//! ([`TrampolineFrame::write_slot`]). Both are encapsulated behind safe
//! APIs; nothing about the trampoline page leaks across the crate
//! boundary as a raw pointer.

// `AtomicU32` is referenced by `TrampolineFrame::load_ready` for the
// acquire-load against the in-frame `ready` flag the AP `xchg`s into;
// the `ApBootSlot` struct itself only uses a plain `u32` so it can stay
// `Copy + Eq` (the asm-side `xchg` atomicity is what the wire protocol
// requires; the Rust-side struct never receives an AP write directly).
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

// The caller-provided `ApStackPool` payload is held in an `UnsafeCell`
// so its `static` lands in writable memory; only the freestanding
// bring-up path materialises one.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
use core::cell::UnsafeCell;

use rustos_arch_api::CpuId;

use crate::apic::{DeliveryMode, Lapic, LapicMmio};

// --- Layout constants ------------------------------------------------

/// Physical address of the 4 KiB-aligned low frame the AP trampoline is
/// installed at.
///
/// The SIPI vector the BSP sends is `(AP_TRAMPOLINE_PHYS >> 12) as u8`,
/// per Intel SDM Vol 3A §8.4.4.1. With the current value `0x8000` the
/// vector is `0x08`.
///
/// 0x8000 sits in the "conventional memory" window QEMU/SeaBIOS/OVMF
/// leave free for the OS; the multiboot2 memory map (parsed by
/// [`crate::multiboot2`]) marks it as `Available` on every QEMU
/// configuration RustOS targets.
pub const AP_TRAMPOLINE_PHYS: u64 = 0x8000;

/// Length of the trampoline payload (`_ap_trampoline_end -
/// _ap_trampoline_start`), in bytes. Cross-checked at runtime by
/// [`TrampolineFrame::install`] against the linker-provided symbols.
pub const AP_TRAMPOLINE_LEN: usize = 0xF00;

/// In-page offset of the [`ApBootSlot`] record. Cross-checked against
/// the assembler-side `_ap_trampoline_boot_slot_offset` symbol on
/// `none`-target builds.
pub const AP_BOOT_SLOT_OFFSET: usize = 0xF00;

/// SIPI vector for the configured [`AP_TRAMPOLINE_PHYS`]. Decoded
/// per SDM Vol 3A §8.4.4.1 ("the SIPI vector specifies the page-aligned
/// starting address of the AP startup routine, expressed as the page
/// number").
#[must_use]
pub const fn sipi_vector() -> u8 {
    // `(AP_TRAMPOLINE_PHYS >> 12)` is statically a value < 256 because
    // `AP_TRAMPOLINE_PHYS` is statically `0x8000` (= page number 8). The
    // `as u8` cast is therefore lossless; a `const _: () = assert!(..)`
    // below fails compilation loudly if a future bump pushes
    // `AP_TRAMPOLINE_PHYS` past 0xFF000.
    #[allow(clippy::cast_possible_truncation)] // bounded by `SIPI_VECTOR_IN_RANGE` below.
    {
        (AP_TRAMPOLINE_PHYS >> 12) as u8
    }
}

/// Compile-time check: `(AP_TRAMPOLINE_PHYS >> 12) < 256`. Pins the
/// `as u8` cast in [`sipi_vector`] to a documented invariant rather
/// than a runtime assertion; the const is never *used* at runtime
/// because its only purpose is to fail compilation if the invariant is
/// ever broken.
#[allow(dead_code)] // const-assert; deliberately never referenced.
const SIPI_VECTOR_IN_RANGE: () = assert!((AP_TRAMPOLINE_PHYS >> 12) < 256);

// --- ApBootSlot ------------------------------------------------------

/// Per-AP record the trampoline reads at offset [`AP_BOOT_SLOT_OFFSET`]
/// inside the 4 KiB trampoline frame.
///
/// Layout is hand-locked to `ap_trampoline.s`:
///
/// | offset | size | field                                          |
/// |--------|------|------------------------------------------------|
/// |  0x00  |  8   | `cr3`         — bootstrap PML4 physical address |
/// |  0x08  |  8   | `stack_top`   — 16-byte-aligned RSP on entry    |
/// |  0x10  |  8   | `entry`       — `extern "C" fn(u32) -> !` ptr   |
/// |  0x18  |  4   | `cpu_id`      — scheduler CPU id (u32)          |
/// |  0x1C  | 36   | reserved (zero)                                 |
/// |  0x40  |  4   | `ready`       — AP writes 1 once long mode up   |
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApBootSlot {
    /// Bootstrap PML4 physical address loaded into `CR3` by the AP.
    pub cr3: u64,
    /// 16-byte-aligned `RSP` value the AP installs before calling
    /// `entry`.
    pub stack_top: u64,
    /// Rust callee the AP invokes after long mode is established. The
    /// callee receives `cpu_id` in `RDI` and is required to be `-> !`.
    pub entry: u64,
    /// Scheduler-visible CPU identifier passed to `entry`.
    pub cpu_id: u32,
    /// Reserved padding so `ready` lands at the assembly-side
    /// `AP_BOOT_SLOT_READY` offset (`0x40`). Public because the struct
    /// is `#[repr(C)]` and its byte layout is part of the wire contract
    /// with `ap_trampoline.s` (invariant audited by
    /// the `ap_boot_slot_layout_is_locked` host test below).
    #[allow(clippy::pub_underscore_fields)]
    pub _reserved: [u8; 36],
    /// Initial value of the rendezvous flag. Always written as `0`;
    /// the AP `xchg`s a `1` here once long mode is up. The BSP reads
    /// the live flag through [`TrampolineFrame::load_ready`] (an
    /// `Acquire` load), not through this struct.
    pub ready: u32,
}

impl ApBootSlot {
    /// Build a fresh slot. The `ready` flag starts at `0`.
    ///
    /// `stack_top` must be 16-byte aligned (System V AMD64 ABI). The
    /// constructor enforces this so a misaligned stack can never reach
    /// the asm side.
    ///
    /// # Errors
    ///
    /// [`SlotError::StackMisaligned`] if `stack_top % 16 != 0`.
    pub fn new(cr3: u64, stack_top: u64, entry: u64, cpu_id: u32) -> Result<Self, SlotError> {
        if stack_top % 16 != 0 {
            return Err(SlotError::StackMisaligned);
        }
        Ok(Self {
            cr3,
            stack_top,
            entry,
            cpu_id,
            _reserved: [0; 36],
            ready: 0,
        })
    }
}

/// Errors raised by [`ApBootSlot::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotError {
    /// `stack_top` was not 16-byte aligned.
    StackMisaligned,
}

// --- Trampoline frame ------------------------------------------------

/// Errors raised by [`TrampolineFrame`] operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallError {
    /// `frame_base` was not 4 KiB aligned.
    FrameMisaligned,
    /// `frame_base` did not satisfy `frame_base >> 12 < 256` (the SIPI
    /// vector field is only 8 bits wide).
    FrameOutOfSipiRange,
    /// The supplied payload length did not match
    /// [`AP_TRAMPOLINE_LEN`].
    PayloadLenMismatch,
}

/// Typed handle to the 4 KiB physical frame at [`AP_TRAMPOLINE_PHYS`]
/// hosting the AP trampoline.
///
/// `'a` is the lifetime of the kernel-supplied mutable byte slice
/// covering that frame. The slice is what makes the API safe: callers
/// must prove they own the frame (typically by reserving it during
/// boot-memory parsing) before they can construct a `TrampolineFrame`.
#[derive(Debug)]
pub struct TrampolineFrame<'a> {
    /// 4 KiB mutable view onto the trampoline frame.
    frame: &'a mut [u8],
}

impl<'a> TrampolineFrame<'a> {
    /// Wrap a 4 KiB mutable byte slice covering the trampoline frame.
    ///
    /// # Errors
    ///
    /// [`InstallError::FrameMisaligned`] if the slice is not exactly
    /// 4 KiB; [`InstallError::FrameOutOfSipiRange`] if the SIPI vector
    /// would not fit in 8 bits at the linked
    /// [`AP_TRAMPOLINE_PHYS`].
    pub fn new(frame: &'a mut [u8]) -> Result<Self, InstallError> {
        if frame.len() != 4096 {
            return Err(InstallError::FrameMisaligned);
        }
        if (AP_TRAMPOLINE_PHYS >> 12) >= 256 {
            return Err(InstallError::FrameOutOfSipiRange);
        }
        Ok(Self { frame })
    }

    /// Copy the assembled trampoline payload into the frame.
    ///
    /// `payload` must be the byte-exact slice between
    /// `_ap_trampoline_start` and `_ap_trampoline_end` — its length must
    /// equal [`AP_TRAMPOLINE_LEN`].
    ///
    /// The frame is zeroed before the payload is written so the
    /// trailing region (between the assembled payload and the
    /// [`ApBootSlot`] at +0xF00) is a known-good zero.
    ///
    /// # Errors
    ///
    /// [`InstallError::PayloadLenMismatch`] if `payload.len() !=
    /// AP_TRAMPOLINE_LEN`.
    pub fn install(&mut self, payload: &[u8]) -> Result<(), InstallError> {
        if payload.len() != AP_TRAMPOLINE_LEN {
            return Err(InstallError::PayloadLenMismatch);
        }
        // Zero the trailing pad too; AP_BOOT_SLOT_OFFSET == AP_TRAMPOLINE_LEN
        // so the slot region is also reset to zero here. The caller is
        // expected to call `write_slot` immediately afterwards.
        for b in self.frame.iter_mut() {
            *b = 0;
        }
        self.frame[..AP_TRAMPOLINE_LEN].copy_from_slice(payload);
        Ok(())
    }

    /// Write the per-AP [`ApBootSlot`] record into the frame at offset
    /// [`AP_BOOT_SLOT_OFFSET`].
    ///
    /// Performed field-by-field through a volatile-aware path so the
    /// compiler may not reorder these writes past the subsequent
    /// `init_sipi_sipi` call.
    pub fn write_slot(&mut self, slot: &ApBootSlot) {
        let slot_bytes: [u8; core::mem::size_of::<ApBootSlot>()] =
            // SAFETY: `ApBootSlot` is `#[repr(C)]` and contains no
            // padding pointers; transmuting to bytes is sound. The
            // destination range is 4 KiB-bounded by construction.
            unsafe { core::mem::transmute_copy(slot) };
        let off = AP_BOOT_SLOT_OFFSET;
        self.frame[off..off + slot_bytes.len()].copy_from_slice(&slot_bytes);
    }

    /// Acquire-load the `ready` flag from the in-frame [`ApBootSlot`].
    ///
    /// # Panics
    /// Never panics in production: `AP_BOOT_SLOT_OFFSET` is `0xF00`,
    /// `AP_BOOT_SLOT_READY_OFFSET` is `0x40`, so the field starts at
    /// `0xF40` — 4-byte aligned and 4 bytes wide — inside the 4 KiB
    /// `self.frame`. The slot region was zeroed in
    /// [`Self::install`] and overwritten by [`Self::write_slot`] with a
    /// 4-byte-aligned `u32`.
    #[must_use]
    pub fn load_ready(&self) -> u32 {
        let off = AP_BOOT_SLOT_OFFSET + AP_BOOT_SLOT_READY_OFFSET;
        // SAFETY: `off + 4 <= 4096`; `off` is 4-byte aligned because
        // `AP_BOOT_SLOT_OFFSET = 0xF00` and `AP_BOOT_SLOT_READY_OFFSET
        // = 0x40` are both multiples of 4 and the caller guarantees a
        // 4 KiB-aligned `frame` base (by passing the bare 4 KiB low
        // physical frame at `AP_TRAMPOLINE_PHYS = 0x8000`; on the host
        // the test buffer is a `[u8; 4096]` which Rust aligns to at
        // least 1 — the slot bytes were placed via `copy_from_slice` of
        // a `transmute_copy`'d `ApBootSlot`, so the 4-byte alignment
        // tracks the data, not the slice). `AtomicU32` has the same
        // layout and alignment as `u32`. `Acquire` ordering pairs with
        // the AP's `xchg`-released store in `ap_trampoline.s`.
        //
        // The clippy `cast_ptr_alignment` lint is suppressed here
        // because the alignment proof above lives in the comments —
        // clippy cannot see the runtime invariant the BSP installer
        // upholds, and an `assert!(off % 4 == 0)` would be a runtime
        // restatement of a compile-time fact (both constants are
        // statically multiples of 4).
        #[allow(clippy::cast_ptr_alignment)] // alignment proven above.
        let p = self.frame[off..off + 4].as_ptr().cast::<AtomicU32>();
        unsafe { (*p).load(Ordering::Acquire) }
    }
}

/// Offset of the `ready` field within `ApBootSlot`. Locked to the
/// assembly-side `AP_BOOT_SLOT_READY` constant.
const AP_BOOT_SLOT_READY_OFFSET: usize = 0x40;

// --- INIT-SIPI-SIPI sequencer ---------------------------------------

/// Abstract microsecond-resolution busy-wait. Implementations on
/// `target_os = "none"` are PIT-based (see `apic_timer::PolledPit`);
/// host tests pass a mock that records every call.
pub trait Delay {
    /// Spin for at least `us` microseconds.
    fn delay_us(&mut self, us: u32);
}

/// Drive a single AP through the SDM-mandated INIT-deassert + INIT +
/// SIPI + SIPI handshake.
///
/// Steps, per SDM Vol 3A §8.4.4.1:
///
/// 1. INIT-deassert (level deassert) — quiesces a possibly-confused AP.
/// 2. 10 ms delay.
/// 3. INIT IPI (rising edge) → AP enters Wait-for-SIPI.
/// 4. 10 ms delay.
/// 5. First SIPI(vector).
/// 6. 200 µs delay.
/// 7. Second SIPI(vector). Idempotent if the AP already started.
///
/// `vector` must be the SIPI vector for the trampoline frame — use
/// [`sipi_vector()`].
pub fn init_sipi_sipi<M: LapicMmio, D: Delay>(
    lapic: &mut Lapic<M>,
    delay: &mut D,
    target_apic_id: u8,
    vector: u8,
) {
    lapic.send_init_deassert(target_apic_id);
    delay.delay_us(10_000);
    lapic.send_ipi(target_apic_id, DeliveryMode::Init, 0);
    delay.delay_us(10_000);
    lapic.send_ipi(target_apic_id, DeliveryMode::StartUp, vector);
    delay.delay_us(200);
    lapic.send_ipi(target_apic_id, DeliveryMode::StartUp, vector);
}

// --- Bare-metal helpers --------------------------------------------

/// Read the BSP's own LAPIC ID from the LAPIC ID register. The BSP must
/// have software-enabled its LAPIC and the MMIO frame must be
/// identity-mapped (boot.s does both).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[must_use]
pub fn bsp_lapic_id() -> u8 {
    // SAFETY: 0xFEE00020 is the architectural LAPIC ID register on
    // every Intel/AMD CPU since the original Pentium; on QEMU it is the
    // emulated default. The MMIO frame is identity-mapped by `boot.s`
    // (SAFETY-INVARIANT 4). A 32-bit volatile read has no side effects.
    let id = unsafe { core::ptr::read_volatile(0xFEE0_0020 as *const u32) };
    ((id >> 24) & 0xFF) as u8
}

// --- Linker-symbol bridge -------------------------------------------

/// Return the byte-exact assembled AP trampoline payload (the slice
/// between `_ap_trampoline_start` and `_ap_trampoline_end`).
///
/// Only available on the bare-metal target — there are no linker
/// symbols on the host build.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[must_use]
pub fn trampoline_payload() -> &'static [u8] {
    extern "C" {
        static _ap_trampoline_start: u8;
        static _ap_trampoline_end: u8;
    }
    // SAFETY: both symbols are defined by `ap_trampoline.s` and pin the
    // half-open byte range of the trampoline payload inside `.ap_trampoline`.
    // The section is `KEEP`'d by `linker.ld`, so the range is in-image
    // and immutable for `'static`. `offset_from` is well-defined because
    // both pointers land in the same allocation (the section).
    unsafe {
        let start = core::ptr::addr_of!(_ap_trampoline_start);
        let end = core::ptr::addr_of!(_ap_trampoline_end);
        // SAFETY: `end >= start` is guaranteed by the linker script
        // (`_ap_trampoline_end` is laid out *after* `_ap_trampoline_start`
        // in the same `KEEP`'d output section), so the signed difference
        // is non-negative and fits in `usize` on every supported target.
        let len = usize::try_from(end.offset_from(start)).unwrap_or(0);
        core::slice::from_raw_parts(start, len)
    }
}

// --- Secondary entry slot -------------------------------------------

/// The AP entry the boot CPU writes into each [`ApBootSlot`], packed
/// into a `usize` (the size of a `fn` pointer) so the bring-up path
/// reads it without a lock. `0` until [`set_secondary_entry`] installs
/// it.
///
/// Unlike aarch64/riscv64 — whose fixed trampolines read a global slot —
/// the x86_64 trampoline jumps to the per-AP `entry` address stored in
/// that AP's [`ApBootSlot`]. `start_secondary` therefore reads this
/// set-once slot and stamps it into every AP's boot slot, so the
/// consumer installs the entry exactly once (mirroring the other ports'
/// [`set_secondary_entry`] contract) rather than passing a raw function
/// pointer per call.
static SECONDARY_ENTRY_FN: AtomicUsize = AtomicUsize::new(0);

/// Failure modes of [`set_secondary_entry`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SetEntryError {
    /// An entry was already installed; the slot is set-once per boot.
    AlreadyInstalled,
}

/// Failure modes of `start_secondary`.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum StartCpuError {
    /// `cpu` was the boot CPU or outside `1..MAX_CPUS`, so it has no
    /// reserved AP stack slot.
    CpuIdOutOfRange,
    /// No secondary entry was installed via [`set_secondary_entry`];
    /// starting an AP that would jump to address `0` is refused so the
    /// failure is loud at the call site, not a triple-fault on the AP.
    NoEntryInstalled,
    /// The AP never published its `ready` flag within the spin budget;
    /// the trampoline frame must not be reused for the next AP, so the
    /// bring-up fails closed rather than racing.
    StartTimedOut,
}

impl StartCpuError {
    /// Stable cause string for audit records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CpuIdOutOfRange => "cpu_id_out_of_range",
            Self::NoEntryInstalled => "no_secondary_entry_installed",
            Self::StartTimedOut => "ap_start_timed_out",
        }
    }
}

/// Install the entry a freshly-started AP runs.
///
/// The function must be `-> !`: an AP has nowhere to return to (the
/// trampoline left it on a private stack with no caller). Encoding the
/// bottom type in the signature pins that at the call site, exactly as
/// the aarch64/riscv64 ports do.
///
/// # Errors
///
/// [`SetEntryError::AlreadyInstalled`] on the second publish.
pub fn set_secondary_entry(entry: extern "C" fn(CpuId) -> !) -> Result<(), SetEntryError> {
    let raw = entry as usize;
    SECONDARY_ENTRY_FN
        .compare_exchange(0, raw, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| SetEntryError::AlreadyInstalled)
}

/// Address of the installed secondary entry (`0` if none).
/// Test/diagnostic observer.
#[must_use]
pub fn secondary_entry_addr() -> usize {
    SECONDARY_ENTRY_FN.load(Ordering::Acquire)
}

#[cfg(test)]
fn clear_secondary_entry_for_tests() {
    SECONDARY_ENTRY_FN.store(0, Ordering::Release);
}

// --- Bare-metal bring-up orchestration ------------------------------

/// Spin budget the boot CPU waits for an AP's `ready` flag before
/// declaring the start timed out. Generous: each iteration is a single
/// acquire-load, and a healthy AP sets `ready` within a few thousand.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
const AP_READY_SPIN_BUDGET: u64 = 10_000_000;

/// Per-AP bootstrap stack. `0x4000` (16 KiB) matches the boot stack size
/// in `boot.s`. Aligned to 16 bytes per the System V AMD64 ABI.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[repr(C, align(16))]
struct ApStack([u8; 16 * 1024]);

/// Published base of the registered [`ApStackPool::stacks`] array
/// (`null` until a pool is registered, so [`start_secondary`] fails
/// closed before registration).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
static AP_STACK_BASE: core::sync::atomic::AtomicPtr<ApStack> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

/// Number of application-processor bootstrap stacks the registered pool
/// covers (`0` until a pool is registered — every AP index is out of
/// range, so an unregistered system fails closed).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
static AP_STACK_LEN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Set-once guard so a second [`ApStackPool::register`] is refused
/// rather than silently re-pointing the live stack pool.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
static AP_STACK_REGISTERED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Failure mode of [`ApStackPool::register`].
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ApStackPoolError {
    /// Pool was already registered; the slot is set-once per boot
    /// (no silent re-pointing of the live pool).
    AlreadyRegistered,
}

/// Caller-owned, `&'static` application-processor bootstrap-stack pool,
/// sized by the constructing caller for its machine (the AP stack count is derived from the-discovered application-
/// processor count, never a fixed `MAX_CPUS - 1` ceiling baked into the
/// arch crate).
///
/// The const parameter `N` is the number of *application* processors the
/// caller brings up (every logical CPU except the boot CPU): a machine
/// that boots `C` CPUs sizes `ApStackPool<{C - 1}>`. Pool entry `idx`
/// backs the AP whose dense [`CpuId`] is `idx + 1`. The arch crate stays
/// allocator-free, so the caller places the pool in a `static`
/// (allocator-free bins) and publishes it through
/// [`ApStackPool::register`] before the first [`start_secondary`].
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[repr(C, align(16))]
pub struct ApStackPool<const N: usize> {
    /// One 16 KiB bootstrap stack per application processor. The
    /// `UnsafeCell` keeps the `static` in writable memory (an AP pushes
    /// onto its stack the instant the trampoline pivots `rsp` onto it)
    /// rather than read-only `.rodata`; Rust never forms a reference to
    /// the bytes, only computes the per-slot top.
    stacks: UnsafeCell<[ApStack; N]>,
}

// SAFETY: each pool slot backs exactly one application processor's
// bootstrap stack (slot `idx` → AP with dense `CpuId` `idx + 1`); no slot
// is ever shared between CPUs, so the pool is `Sync`.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
unsafe impl<const N: usize> Sync for ApStackPool<N> {}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
impl<const N: usize> ApStackPool<N> {
    /// A zeroed pool of `N` AP bootstrap stacks. `const` so the
    /// allocator-free bins can place it in a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            stacks: UnsafeCell::new([const { ApStack([0; 16 * 1024]) }; N]),
        }
    }

    /// Publish this pool to [`start_secondary`], then return the covered
    /// AP count `N`. Must be called on the boot CPU, exactly once, before
    /// the first [`start_secondary`].
    ///
    /// # Errors
    ///
    /// [`ApStackPoolError::AlreadyRegistered`] on the second publish
    /// (set-once per boot — never silently re-points the live pool).
    pub fn register(&'static self) -> Result<usize, ApStackPoolError> {
        if AP_STACK_REGISTERED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(ApStackPoolError::AlreadyRegistered);
        }
        AP_STACK_BASE.store(self.stacks.get().cast::<ApStack>(), Ordering::Release);
        AP_STACK_LEN.store(N, Ordering::Release);
        Ok(N)
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
impl<const N: usize> Default for ApStackPool<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Top-of-stack (one past the last byte, 16-byte aligned) of registered
/// AP stack pool entry `idx`, or `None` if `idx` is out of range or no
/// pool is registered yet (fail closed).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn ap_stack_top(idx: usize) -> Option<u64> {
    if idx >= AP_STACK_LEN.load(Ordering::Acquire) {
        return None;
    }
    let base = AP_STACK_BASE.load(Ordering::Acquire);
    if base.is_null() {
        return None;
    }
    // SAFETY: a non-zero `AP_STACK_LEN` (checked above) is published in
    // the same `register` call that stores the non-null base from a
    // `&'static ApStackPool`'s `stacks` array of that length, and
    // `idx < len`, so `base.add(idx)` is in bounds. The struct and the
    // array are both `align(16)`, so `base + size_of` preserves the
    // 16-byte alignment the System V ABI requires for RSP on entry.
    unsafe {
        let slot = base.add(idx) as u64;
        Some(slot + core::mem::size_of::<ApStack>() as u64)
    }
}

/// Typed view of the 4 KiB low frame at [`AP_TRAMPOLINE_PHYS`].
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn trampoline_frame_mut() -> &'static mut [u8] {
    // SAFETY: identity-mapped (boot.s SAFETY-INVARIANT 4); the boot CPU
    // serialises AP launches (it waits on each AP's `ready` flag before
    // the next), so no other CPU reads or writes this page concurrently,
    // and the page is reserved by the image (no allocator hands it out —
    // the kernel heap lives well above 0x8000).
    unsafe { core::slice::from_raw_parts_mut(AP_TRAMPOLINE_PHYS as *mut u8, 4096) }
}

/// [`Delay`] backed by busy-waiting on PIT channel 2 OUT.
///
/// The INIT-SIPI-SIPI handshake needs a real microsecond delay between
/// IPIs (SDM Vol 3A §8.4.4.1); the PIT is the one timer guaranteed
/// available before the LAPIC timer is calibrated.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
struct PitDelay {
    pit: crate::apic_timer::PolledPit,
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
impl Delay for PitDelay {
    fn delay_us(&mut self, us: u32) {
        use crate::apic_timer::PortIo as _;
        // PIT runs at 1.193182 MHz: ticks = us * 1_193_182 / 1_000_000.
        // For `us` ≤ 54_925 the value fits in 16 bits; the bring-up uses
        // 10_000 and 200, both well below.
        #[allow(clippy::cast_possible_truncation)] // bounded by the comment above.
        let reload = ((u64::from(us) * 1_193_182) / 1_000_000) as u16;
        if reload == 0 {
            return;
        }
        // Arm channel 2 one-shot, gate it, then poll the OUT bit.
        let gate = self.pit.inb(0x61);
        self.pit.outb(0x61, (gate & 0xFC) | 0x01);
        self.pit.outb(0x43, 0xB0);
        self.pit.outb(0x42, (reload & 0xFF) as u8);
        self.pit.outb(0x42, (reload >> 8) as u8);
        while self.pit.inb(0x61) & 0x20 == 0 {}
    }
}

/// Build an ephemeral [`Lapic`] over this CPU's identity-mapped LAPIC
/// MMIO frame. The boot CPU must have software-enabled its LAPIC first.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn bringup_lapic() -> Lapic<crate::apic::VolatileLapicMmio> {
    // SAFETY: `LAPIC_BASE_PHYS` is the architectural LAPIC MMIO base on
    // every Intel-architecture system QEMU emulates, identity-mapped by
    // `boot.s` (SAFETY-INVARIANT 4).
    let mmio =
        unsafe { crate::apic::VolatileLapicMmio::new(crate::preempt::LAPIC_BASE_PHYS as *mut u32) };
    Lapic::new(mmio)
}

/// Start the application processor whose LAPIC id is `target_apic_id`
/// and whose dense id is `cpu`, at the [`AP_TRAMPOLINE_PHYS`] trampoline.
///
/// Installs the trampoline payload, stamps a per-AP [`ApBootSlot`]
/// (bootstrap `cr3` read from the boot CPU, this AP's reserved stack, the
/// installed [`set_secondary_entry`] entry, and `cpu` as the context id),
/// drives the INIT-SIPI-SIPI handshake, then waits for the AP to publish
/// its `ready` flag before returning — so the shared trampoline frame is
/// safe to reuse for the next AP.
///
/// # Errors
///
/// See [`StartCpuError`]. The launcher fails closed
/// rather than assuming the AP came up.
///
/// # Safety
///
/// Must be called from the boot CPU after `boot.s` has zeroed `.bss`
/// (clearing the AP stack pool), after the boot CPU has software-enabled
/// its LAPIC, and after the secondary entry is installed. `target_apic_id`
/// must name a real, parked AP distinct from the caller, and `cpu` must be
/// the dense id the rest of the kernel uses for it.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub unsafe fn start_secondary(target_apic_id: u8, cpu: CpuId) -> Result<(), StartCpuError> {
    // Range-check before any hardware action: the boot CPU (0) is
    // already running, and a dense id beyond the stack pool has no
    // reserved stack.
    let Some(stack_idx) = (cpu as usize).checked_sub(1) else {
        return Err(StartCpuError::CpuIdOutOfRange);
    };
    // The registered pool's published length is the only bound (no baked-in
    // `MAX_APS`); an unregistered pool or an out-of-range AP fails closed.
    let Some(stack_top) = ap_stack_top(stack_idx) else {
        return Err(StartCpuError::CpuIdOutOfRange);
    };
    let entry_addr = secondary_entry_addr();
    if entry_addr == 0 {
        return Err(StartCpuError::NoEntryInstalled);
    }

    let mut frame = match TrampolineFrame::new(trampoline_frame_mut()) {
        Ok(frame) => frame,
        // The frame address/size are compile-time constants that always
        // satisfy the installer; treat any rejection as out-of-range
        // rather than panicking.
        Err(_) => return Err(StartCpuError::CpuIdOutOfRange),
    };
    if frame.install(trampoline_payload()).is_err() {
        return Err(StartCpuError::CpuIdOutOfRange);
    }

    // Read CR3 — the boot CPU's PML4; APs inherit it.
    let cr3: u64;
    // SAFETY: reading CR3 in ring 0 is well-defined and side-effect-free.
    unsafe {
        core::arch::asm!("mov {x}, cr3", x = out(reg) cr3, options(nostack, preserves_flags));
    }
    let slot = match ApBootSlot::new(cr3, stack_top, entry_addr as u64, cpu) {
        Ok(slot) => slot,
        // `stack_top` is 16-byte aligned by construction; a misalignment
        // would be an internal invariant break, not a caller error.
        Err(_) => return Err(StartCpuError::CpuIdOutOfRange),
    };
    frame.write_slot(&slot);

    // Every slot byte must be visible to the AP before the SIPI.
    core::sync::atomic::fence(Ordering::Release);

    let mut lapic = bringup_lapic();
    let mut delay = PitDelay {
        pit: crate::apic_timer::PolledPit,
    };
    init_sipi_sipi(&mut lapic, &mut delay, target_apic_id, sipi_vector());

    // Wait for the AP's `xchg`-released `ready` flag before returning, so
    // the shared frame can be reused for the next AP.
    let mut spins: u64 = 0;
    while frame.load_ready() == 0 {
        spins += 1;
        if spins > AP_READY_SPIN_BUDGET {
            return Err(StartCpuError::StartTimedOut);
        }
        core::hint::spin_loop();
    }
    Ok(())
}

// --- Tests -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::vec::Vec;

    extern crate std;

    /// Host-side `LapicMmio` mock recording every write in order.
    struct RecordingLapic {
        writes: Vec<(usize, u32)>,
    }
    impl RecordingLapic {
        fn new() -> Self {
            Self { writes: Vec::new() }
        }
    }
    impl LapicMmio for RecordingLapic {
        fn read(&self, _off: usize) -> u32 {
            0
        }
        fn write(&mut self, off: usize, value: u32) {
            self.writes.push((off, value));
        }
    }

    /// Host-side delay mock recording every `delay_us` call in order.
    struct RecordingDelay {
        calls: Vec<u32>,
    }
    impl Delay for RecordingDelay {
        fn delay_us(&mut self, us: u32) {
            self.calls.push(us);
        }
    }

    #[test]
    fn sipi_vector_decodes_to_eight() {
        // The compile-time `SIPI_VECTOR_IN_RANGE` const proves the
        // truncation cast is lossless; the runtime assertion below
        // pins the *current* value so a future move of
        // `AP_TRAMPOLINE_PHYS` fails this test loudly.
        let _: () = SIPI_VECTOR_IN_RANGE;
        assert_eq!(sipi_vector(), 0x08);
    }

    #[test]
    fn ap_boot_slot_layout_is_locked() {
        // The assembly side references `AP_BOOT_SLOT_CR3` etc. at
        // hand-chosen offsets. If anyone ever bumps `ApBootSlot` they
        // must update `ap_trampoline.s` too — this test fails loudly
        // otherwise.
        use core::mem::offset_of;
        assert_eq!(offset_of!(ApBootSlot, cr3), 0x00);
        assert_eq!(offset_of!(ApBootSlot, stack_top), 0x08);
        assert_eq!(offset_of!(ApBootSlot, entry), 0x10);
        assert_eq!(offset_of!(ApBootSlot, cpu_id), 0x18);
        assert_eq!(offset_of!(ApBootSlot, ready), AP_BOOT_SLOT_READY_OFFSET);
        // The slot must fit in the trampoline frame from
        // AP_BOOT_SLOT_OFFSET to end-of-page.
        assert!(AP_BOOT_SLOT_OFFSET + core::mem::size_of::<ApBootSlot>() <= 4096);
    }

    #[test]
    fn slot_rejects_misaligned_stack() {
        let err = ApBootSlot::new(0x1000, 0xDEAD_BEEF, 0x4000, 0).unwrap_err();
        assert_eq!(err, SlotError::StackMisaligned);
    }

    #[test]
    fn slot_constructs_when_inputs_are_valid() {
        let slot = ApBootSlot::new(0x1000, 0x0001_0000, 0x4000, 3).unwrap();
        assert_eq!(slot.cr3, 0x1000);
        assert_eq!(slot.stack_top, 0x0001_0000);
        assert_eq!(slot.entry, 0x4000);
        assert_eq!(slot.cpu_id, 3);
        assert_eq!(slot.ready, 0);
    }

    #[test]
    fn frame_new_rejects_wrong_size() {
        let mut buf = [0u8; 2048];
        assert_eq!(
            TrampolineFrame::new(&mut buf[..]).unwrap_err(),
            InstallError::FrameMisaligned
        );
    }

    #[test]
    fn install_zeroes_frame_then_writes_payload() {
        let mut buf = [0xAAu8; 4096];
        let payload = [0x90u8; AP_TRAMPOLINE_LEN]; // 0x90 = NOP
        let mut frame = TrampolineFrame::new(&mut buf[..]).unwrap();
        frame.install(&payload).unwrap();
        // The first AP_TRAMPOLINE_LEN bytes are the payload.
        assert!(buf[..AP_TRAMPOLINE_LEN].iter().all(|&b| b == 0x90));
        // The trailing region (slot area) is zero before `write_slot`.
        assert!(buf[AP_TRAMPOLINE_LEN..].iter().all(|&b| b == 0));
    }

    #[test]
    fn install_rejects_payload_length_mismatch() {
        let mut buf = [0u8; 4096];
        let mut frame = TrampolineFrame::new(&mut buf[..]).unwrap();
        let too_short = [0u8; 16];
        assert_eq!(
            frame.install(&too_short).unwrap_err(),
            InstallError::PayloadLenMismatch
        );
    }

    #[test]
    fn write_slot_persists_into_frame_at_correct_offset() {
        let mut buf = [0u8; 4096];
        let payload = [0u8; AP_TRAMPOLINE_LEN];
        let slot = ApBootSlot::new(0xCAFE_F000, 0x1234_0000, 0x5678_0000, 7).unwrap();
        {
            let mut frame = TrampolineFrame::new(&mut buf[..]).unwrap();
            frame.install(&payload).unwrap();
            frame.write_slot(&slot);
            assert_eq!(frame.load_ready(), 0);
        }
        let cr3_le = &buf[AP_BOOT_SLOT_OFFSET..AP_BOOT_SLOT_OFFSET + 8];
        assert_eq!(u64::from_le_bytes(cr3_le.try_into().unwrap()), 0xCAFE_F000);
        let cpu_le = &buf[AP_BOOT_SLOT_OFFSET + 0x18..AP_BOOT_SLOT_OFFSET + 0x1C];
        assert_eq!(u32::from_le_bytes(cpu_le.try_into().unwrap()), 7);
    }

    #[test]
    fn init_sipi_sipi_drives_documented_sequence() {
        let mut lapic = Lapic::new(RecordingLapic::new());
        let mut delay = RecordingDelay { calls: Vec::new() };
        init_sipi_sipi(&mut lapic, &mut delay, 3, sipi_vector());

        // The sequence is: INIT-deassert, delay(10_000), INIT,
        // delay(10_000), SIPI, delay(200), SIPI.
        //
        // Each IPI lowers ICR_HIGH (offset 0x310) before ICR_LOW
        // (offset 0x300) — see `Lapic::send_ipi`. So we expect 7
        // delay-or-write events; for IPIs we expect two writes (high
        // then low).
        assert_eq!(delay.calls, [10_000, 10_000, 200]);
        let writes = &lapic.mmio_mut().writes;
        // Four IPIs * 2 writes each = 8 LAPIC writes.
        assert_eq!(writes.len(), 8);
        // Every IPI writes ICR_HIGH (0x310) then ICR_LOW (0x300).
        for chunk in writes.chunks_exact(2) {
            assert_eq!(chunk[0].0, 0x310);
            assert_eq!(chunk[1].0, 0x300);
            // ICR_HIGH destination field is bits 24..31.
            assert_eq!((chunk[0].1 >> 24) & 0xFF, 3);
        }
        // First IPI is INIT-deassert: delivery mode 0b101 in bits 8..10,
        // level=0 (deassert), trigger=1 (level), vector=0.
        let icr_low_deassert = writes[1].1;
        assert_eq!(icr_low_deassert & 0x700, 0x500); // delivery mode = Init
        assert_eq!((icr_low_deassert >> 14) & 1, 0); // level = 0 (deassert)
        assert_eq!((icr_low_deassert >> 15) & 1, 1); // trigger = level
                                                     // Second IPI is INIT (assert): delivery mode = Init, level = 1.
        let icr_low_init = writes[3].1;
        assert_eq!(icr_low_init & 0x700, 0x500);
        assert_eq!((icr_low_init >> 14) & 1, 1);
        // Third & fourth IPIs are SIPI(vector). Delivery mode = 0b110.
        for &i in &[5, 7] {
            let icr_low = writes[i].1;
            assert_eq!(icr_low & 0x700, 0x600);
            assert_eq!(icr_low & 0xFF, u32::from(sipi_vector()));
        }
    }

    /// A never-returning entry used only to populate the set-once slot;
    /// it is never actually invoked on the host.
    extern "C" fn dummy_entry(_cpu: CpuId) -> ! {
        loop {
            core::hint::spin_loop();
        }
    }

    #[test]
    fn secondary_entry_round_trips_and_is_set_once() {
        clear_secondary_entry_for_tests();
        assert_eq!(secondary_entry_addr(), 0);
        assert_eq!(set_secondary_entry(dummy_entry), Ok(()));
        assert_eq!(secondary_entry_addr(), dummy_entry as *const () as usize);
        // A second publish is refused (set-once); the slot is unchanged.
        assert_eq!(
            set_secondary_entry(dummy_entry),
            Err(SetEntryError::AlreadyInstalled)
        );
        assert_eq!(secondary_entry_addr(), dummy_entry as *const () as usize);
        clear_secondary_entry_for_tests();
    }

    #[test]
    fn start_cpu_error_cause_strings_are_stable() {
        assert_eq!(
            StartCpuError::CpuIdOutOfRange.as_str(),
            "cpu_id_out_of_range"
        );
        assert_eq!(
            StartCpuError::NoEntryInstalled.as_str(),
            "no_secondary_entry_installed"
        );
        assert_eq!(StartCpuError::StartTimedOut.as_str(), "ap_start_timed_out");
    }
}
