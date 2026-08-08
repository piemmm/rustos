//! Per-CPU GDT + TSS + IST tables (Stage 3a (c) — partial).
//!
//! This module owns the *types and builders* every CPU needs to replace
//! the trampoline-internal GDT (`ap_trampoline.s`) and the boot-time GDT
//! (`boot.s`) with a real, per-CPU long-mode descriptor table that
//!
//!   * carries kernel and user code/data segments at fixed indices,
//!   * carries a CPU-private TSS so `ltr` works and the CPU can switch
//!     to a known-good stack on an interrupt (`RSP0`),
//!   * carries up to seven IST stacks so `#DF`, `#NMI`, etc. can land on
//!     a stack that is *guaranteed* to be present and 16-byte aligned
//!     even if the running task's stack is corrupt.
//!
//! What this module does *not* do, and is deliberately split out into
//! the remaining Stage 3a (c) follow-ups in
//! `.junie/next-session-prompt.md`:
//!
//!   * Drive AP bring-up: that lives in `kernel/arch/x86_64/src/smp.rs`.
//!   * Define the ISR prologue / epilogue: the IST slots here are
//!     *passive* — they merely tell the CPU where to land. Wiring an
//!     ISR vector to `IST1`/`IST2` is part of the context-switch /
//!     interrupt-entry commit.
//!   * Implement `syscall`/`sysret` MSR programming. The `USER_CS` /
//!     `USER_DS` selectors are positioned at the indices `IA32_STAR`
//!     requires (kernel selectors at `STAR[47:32]`, user selectors at
//!     `STAR[63:48] - 16`) but `IA32_STAR` itself is not written here.
//!
//! # Layout
//!
//! Long-mode descriptor tables are still arrays of 8-byte segment
//! descriptors. Code/data descriptors in 64-bit mode collapse most of
//! the legacy `base`/`limit` fields (the CPU ignores them, per Intel
//! SDM Vol 3A §3.4.5); only `L` (long mode), `DPL`, `P`, and the
//! type bits remain meaningful. The TSS descriptor in 64-bit mode is
//! the architecturally mandated *16-byte* system descriptor (SDM Vol
//! 3A §8.2.3 Figure 8-4), so it occupies two consecutive GDT slots.
//!
//! Slot layout:
//!
//! | Index | Selector | Descriptor                                |
//! |-------|----------|-------------------------------------------|
//! |   0   | `0x00`   | Null                                      |
//! |   1   | `0x08`   | Kernel code (CS), DPL=0, L=1, P=1         |
//! |   2   | `0x10`   | Kernel data (SS/DS), DPL=0, P=1           |
//! |   3   | `0x18`   | User data,            DPL=3, P=1          |
//! |   4   | `0x20`   | User code (CS),       DPL=3, L=1, P=1     |
//! |   5+6 | `0x28`   | TSS (16-byte system descriptor)           |
//!
//! The user CS sits *after* user DS deliberately: `IA32_STAR[63:48]`
//! programs the `sysret` selector base; the CPU then derives the
//! sysret CS as `STAR[63:48] + 16` and the sysret SS as
//! `STAR[63:48] + 8` (SDM Vol 2B §SYSRET). With kernel CS at index 1
//! and user data at index 3, programming `STAR` with selector base
//! `0x18` will resolve sysret SS = `0x20` (user data) and sysret CS =
//! `0x28` (user code). The TSS therefore must sit at index 5/6, *not*
//! immediately after the kernel descriptors.
//!
//! # `unsafe` discipline
//!
//! Construction and IST registration are entirely safe; they take
//! kernel-supplied stack-top pointers and validate them. The only
//! `unsafe` surface is the bare-metal `PerCpuGdt::install` function
//! (gated behind `cfg(all(target_arch = "x86_64", target_os = "none"))`)
//! which
//! executes `lgdt`, reloads segment registers via a far return, and
//! issues `ltr`. Every `unsafe` block carries a `// SAFETY:` block; the
//! host unit tests below cover every non-asm invariant.

use core::mem::size_of;

// --- Public layout constants ----------------------------------------

/// Number of 8-byte slots in the per-CPU GDT (null + 4 segments + 16-
/// byte TSS = 7 slots).
pub const GDT_SLOTS: usize = 7;

/// Index of the kernel code segment. Selector = `KERNEL_CS_INDEX << 3`.
pub const KERNEL_CS_INDEX: u16 = 1;

/// Index of the kernel data segment. Selector = `KERNEL_DS_INDEX << 3`.
pub const KERNEL_DS_INDEX: u16 = 2;

/// Index of the user data segment. Sits *before* user code so that
/// `IA32_STAR[63:48]` programmed with `USER_DS_INDEX << 3` resolves
/// `sysret` SS/CS correctly (see module-level layout note).
pub const USER_DS_INDEX: u16 = 3;

/// Index of the user code segment.
pub const USER_CS_INDEX: u16 = 4;

/// Index of the first of the two 8-byte slots making up the 64-bit TSS
/// descriptor. The second slot is `TSS_INDEX + 1`.
pub const TSS_INDEX: u16 = 5;

/// Number of IST stacks the TSS carries. Architecturally fixed at 7
/// (SDM Vol 3A §8.7: `IST1`..`IST7`; slot 0 is reserved).
pub const IST_COUNT: usize = 7;

/// Required alignment of an IST stack top pointer in bytes (System V
/// AMD64 ABI: 16-byte aligned at entry to a function; the CPU pushes
/// the exception frame onto the IST stack and the frame layout
/// requires 16-byte alignment of the resulting stack pointer per SDM
/// Vol 3A §6.14.5).
pub const IST_STACK_ALIGN: u64 = 16;

// --- 8-byte segment descriptor --------------------------------------

/// One 8-byte GDT slot. Wraps the raw `u64` so callers cannot
/// accidentally treat a segment descriptor as a `u64` or vice versa.
///
/// 64-bit long-mode code/data descriptors use a *very* reduced set of
/// the legacy fields — the CPU ignores base and limit (Intel SDM Vol
/// 3A §3.4.5) — so the constructors below only encode the bits the
/// architecture still consults. The full 64-bit raw value is exposed
/// via [`Self::raw`] for the host test suite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct GdtEntry(u64);

impl GdtEntry {
    /// The all-zero (null) descriptor at slot 0.
    pub const NULL: Self = Self(0);

    /// Build a kernel code segment descriptor: P=1, DPL=0, S=1,
    /// Type=Code (0xA — execute/read, non-conforming), L=1, D=0.
    ///
    /// Encoded bits (Intel SDM Vol 3A §3.4.5 Figure 3-8):
    ///
    /// * `Type[40..44]   = 0xA` (code, execute/read)
    /// * `S   [44]       = 1`   (code/data, not system)
    /// * `DPL [45..47]   = 0`   (kernel)
    /// * `P   [47]       = 1`   (present)
    /// * `L   [53]       = 1`   (long mode)
    /// * `D/B [54]       = 0`   (must be 0 when L=1)
    #[must_use]
    pub const fn kernel_code() -> Self {
        // bit positions:
        //   Type=0xA -> bits 40..44 = 0xA
        //   S=1      -> bit 44
        //   DPL=0    -> bits 45..47
        //   P=1      -> bit 47
        //   L=1      -> bit 53
        Self((0xA << 40) | (1 << 44) | (1 << 47) | (1 << 53))
    }

    /// Build a kernel data segment descriptor: P=1, DPL=0, S=1,
    /// Type=Data writable (0x2 — read/write, expand-up).
    #[must_use]
    pub const fn kernel_data() -> Self {
        Self((0x2 << 40) | (1 << 44) | (1 << 47))
    }

    /// Build a user code segment descriptor: P=1, DPL=3, S=1,
    /// Type=Code (0xA), L=1.
    #[must_use]
    pub const fn user_code() -> Self {
        Self((0xA << 40) | (1 << 44) | (3 << 45) | (1 << 47) | (1 << 53))
    }

    /// Build a user data segment descriptor: P=1, DPL=3, S=1,
    /// Type=Data writable (0x2).
    #[must_use]
    pub const fn user_data() -> Self {
        Self((0x2 << 40) | (1 << 44) | (3 << 45) | (1 << 47))
    }

    /// Raw 64-bit value of the descriptor.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

// --- 16-byte TSS descriptor -----------------------------------------

/// Split a 64-bit TSS base address into the two 8-byte system
/// descriptor slots required by 64-bit mode (SDM Vol 3A §8.2.3 Figure
/// 8-4).
///
/// `limit` is the byte-length-minus-one of the TSS region.
/// `dpl` is the descriptor privilege level (always 0 for a kernel TSS;
/// userspace cannot `ltr`).
///
/// Returns `[low, high]` 8-byte values. The low slot encodes:
///
/// * `Limit[0..16]    = limit[0..16]`
/// * `Base [16..40]   = base[0..24]`
/// * `Type[40..44]    = 0x9` (64-bit TSS, available)
/// * `S   [44]        = 0`   (system descriptor)
/// * `DPL [45..47]    = dpl`
/// * `P   [47]        = 1`
/// * `Limit[48..52]   = limit[16..20]`
/// * `G   [55]        = 0` — byte granularity. The TSS fits in well under 1 MiB so no need for 4 KiB granularity.
/// * `Base[56..64]    = base[24..32]`
///
/// The high slot encodes:
///
/// * `Base[0..32]     = base[32..64]`
/// * `Reserved[32..64]= 0`
#[must_use]
pub const fn tss_descriptor(base: u64, limit: u32, dpl: u8) -> [u64; 2] {
    // Mask the user-supplied DPL down to 2 bits so a misuse cannot
    // smear into the P bit. A real misuse is still a kernel bug but we
    // fail closed instead of producing a malformed descriptor.
    let dpl = (dpl & 0x3) as u64;
    let limit = limit as u64;
    let base_lo24 = base & 0x00FF_FFFF;
    let base_mid8 = (base >> 24) & 0xFF;
    let limit_lo16 = limit & 0xFFFF;
    let limit_hi4 = (limit >> 16) & 0xF;

    let low = limit_lo16
        | (base_lo24 << 16)
        | (0x9_u64 << 40)
        | (dpl << 45)
        | (1_u64 << 47)
        | (limit_hi4 << 48)
        | (base_mid8 << 56);

    let high = (base >> 32) & 0xFFFF_FFFF;
    [low, high]
}

// --- 64-bit TSS structure -------------------------------------------

/// 64-bit Task State Segment.
///
/// Layout per Intel SDM Vol 3A §8.7 Figure 8-11. Only the fields the
/// kernel actually programs are public; everything else is held in
/// `_reserved*` arrays to lock the offsets at compile time. The host
/// unit tests below cross-check the offset of every public field
/// against the SDM.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct Tss {
    _reserved0: u32,
    /// `RSP0`..`RSP2`: stack pointer the CPU loads on a privilege-level
    /// transition into ring `n`.
    pub privilege_stack: [u64; 3],
    _reserved1: u64,
    /// `IST1`..`IST7`. `ist_stack[0]` is `IST1`. IST 0 (the "no IST"
    /// value the IDT vector encodes when it doesn't want a stack swap)
    /// is *not* a TSS field.
    pub ist_stack: [u64; 7],
    _reserved2: u64,
    _reserved3: u16,
    /// I/O permission bitmap base offset, measured from the start of
    /// the TSS. The kernel sets this to `size_of::<Tss>()` so the
    /// bitmap is effectively absent (every port-I/O attempt from ring
    /// 3 then `#GP`s).
    pub iopb: u16,
}

impl Tss {
    /// Build an all-zero TSS with the I/O permission base pointing
    /// past the end of the structure (i.e. no port-I/O permission
    /// bitmap; every userspace `in`/`out` traps to `#GP`).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            _reserved0: 0,
            privilege_stack: [0; 3],
            _reserved1: 0,
            ist_stack: [0; 7],
            _reserved2: 0,
            _reserved3: 0,
            // Effectively no IOPB: the CPU only consults the bitmap if
            // the offset is *less than* the segment limit (the TSS
            // limit is encoded in the TSS descriptor and set by
            // `PerCpuGdt::install`). Pointing past the limit is the
            // documented "no IOPB" idiom.
            //
            // Justification for the `as u16` cast (clippy
            // `cast_possible_truncation` would otherwise fire):
            // `TSS_BYTE_LEN` const-asserts that `size_of::<Tss>()` is
            // exactly `0x68`, which fits in a `u16` without loss.
            #[allow(clippy::cast_possible_truncation)]
            // — see `TSS_BYTE_LEN` invariant above.
            iopb: size_of::<Tss>() as u16,
        }
    }
}

impl Default for Tss {
    fn default() -> Self {
        Self::new()
    }
}

/// Byte length of [`Tss`], expressed as a `u32` so the segment-limit
/// arithmetic in [`PerCpuGdt::finalize`] does not need a `usize→u32`
/// cast that clippy would (rightly) flag as potentially-truncating.
///
/// The const-assertion immediately below pins this value to the SDM-
/// dictated `0x68`; any future struct edit that changes the in-memory
/// layout fails compilation here rather than silently producing a
/// malformed TSS descriptor.
pub const TSS_BYTE_LEN: u32 = 0x68;

/// Compile-time check that [`TSS_BYTE_LEN`] tracks [`Tss`]'s real
/// layout. Never referenced at runtime.
#[allow(dead_code)] // const-assert; deliberately never used at runtime.
const TSS_BYTE_LEN_MATCHES_LAYOUT: () = assert!(size_of::<Tss>() == TSS_BYTE_LEN as usize);

// --- IST configuration error ----------------------------------------

/// Errors raised by [`PerCpuGdt::set_ist`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IstError {
    /// The 1-based IST index was outside `1..=IST_COUNT`.
    IndexOutOfRange,
    /// The supplied stack-top pointer was not 16-byte aligned.
    Misaligned,
    /// The supplied stack-top pointer was zero (treated as "absent" by
    /// the CPU and almost certainly a kernel bug).
    NullPointer,
}

// --- GDT integrity validation ---------------------------------------

/// Reasons [`PerCpuGdt::validate`] rejects an in-memory table.
///
/// Each variant is a privilege-boundary invariant a corrupted (or
/// mis-built) descriptor table would break (of
/// the security charter — call-gate / IST CVE classes). Validation runs
/// against the bytes in memory *before* `lgdt`/`ltr` install them, so a
/// scribbled table fails closed rather than being loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GdtError {
    /// Slot 0 was not the all-zero null descriptor the CPU requires.
    NullSlotNotZero,
    /// A segment slot did not have its present (`P`) bit set.
    SegmentNotPresent {
        /// GDT slot index of the offending descriptor.
        index: u16,
    },
    /// A segment slot had its system (`S=0`) bit, i.e. it is not a
    /// code/data segment where one is required.
    NotACodeOrDataSegment {
        /// GDT slot index of the offending descriptor.
        index: u16,
    },
    /// A kernel segment (kernel CS/DS) was not at DPL 0 — a privilege
    /// downgrade that would let ring 3 reach a kernel selector.
    KernelSegmentNotDpl0 {
        /// GDT slot index of the offending descriptor.
        index: u16,
    },
    /// A user segment (user CS/DS) was not at DPL 3.
    UserSegmentNotDpl3 {
        /// GDT slot index of the offending descriptor.
        index: u16,
    },
    /// The TSS descriptor was not a present, available 64-bit TSS
    /// (type `0x9`, `S=0`, `P=1`) — i.e. the table was never finalized
    /// or its TSS slot was corrupted.
    TssDescriptorMalformed,
    /// The TSS descriptor's base address did not point at this
    /// `PerCpuGdt`'s own [`Tss`].
    TssBaseMismatch,
    /// `RSP0` (the ring-0 stack the CPU loads on a ring 3 → ring 0
    /// transition) was null or not 16-byte aligned — an interrupt taken
    /// from user mode would pivot onto an unusable stack.
    PrivilegeStackInvalid,
    /// A registered (non-zero) IST stack top was not 16-byte aligned.
    IstPointerMisaligned {
        /// 1-based IST index (`IST1`..`IST7`).
        index: u8,
    },
}

/// Descriptor privilege level (`DPL`, bits 45..47) of a segment slot.
const fn descriptor_dpl(d: u64) -> u64 {
    (d >> 45) & 0x3
}

/// Reconstruct a 64-bit TSS base from its two descriptor slots — the
/// inverse of [`tss_descriptor`]'s base split (SDM Vol 3A §8.2.3).
const fn tss_base_from_descriptor(low: u64, high: u64) -> u64 {
    let base_lo24 = (low >> 16) & 0x00FF_FFFF;
    let base_mid8 = (low >> 56) & 0xFF;
    let base_hi32 = high & 0xFFFF_FFFF;
    base_lo24 | (base_mid8 << 24) | (base_hi32 << 32)
}

// --- Per-CPU GDT -----------------------------------------------------

/// Per-CPU GDT + TSS bundle.
///
/// Holds owned storage for the 7-slot GDT and a single 64-bit TSS. The
/// caller is responsible for placing each [`PerCpuGdt`] in
/// `'static`-lifetime memory before `PerCpuGdt::install` is called:
/// `lgdt` records the GDT *physical/linear address*, so the storage
/// must outlive every later context switch.
///
/// The structure is intentionally `Copy`-free and `Sync`-free: a GDT
/// belongs to exactly one CPU and is mutated only at install time on
/// that CPU.
#[repr(C)]
#[derive(Debug)]
pub struct PerCpuGdt {
    /// 7 × 8 = 56 bytes of descriptor slots. Public for the host test
    /// suite; production code goes through the typed accessors below.
    pub entries: [u64; GDT_SLOTS],
    /// The TSS this GDT's TSS descriptor (slots `TSS_INDEX`,
    /// `TSS_INDEX + 1`) points at.
    pub tss: Tss,
}

impl PerCpuGdt {
    /// Build a fresh per-CPU GDT with the four canonical kernel/user
    /// segments populated. The TSS descriptor slots are left zero
    /// until [`Self::finalize`] is called (which needs the structure
    /// to be at its final `'static` address so the descriptor can
    /// embed its linear base).
    #[must_use]
    pub const fn new() -> Self {
        let mut entries = [0_u64; GDT_SLOTS];
        entries[KERNEL_CS_INDEX as usize] = GdtEntry::kernel_code().raw();
        entries[KERNEL_DS_INDEX as usize] = GdtEntry::kernel_data().raw();
        entries[USER_DS_INDEX as usize] = GdtEntry::user_data().raw();
        entries[USER_CS_INDEX as usize] = GdtEntry::user_code().raw();
        Self {
            entries,
            tss: Tss::new(),
        }
    }

    /// Register a kernel stack the CPU will load into `RSP` on a
    /// privilege-level transition into ring `ring`.
    ///
    /// `ring` is `0..=2`; `stack_top` must be 16-byte aligned and
    /// non-null.
    ///
    /// # Errors
    ///
    /// [`IstError::IndexOutOfRange`] if `ring > 2`,
    /// [`IstError::Misaligned`] if `stack_top % 16 != 0`,
    /// [`IstError::NullPointer`] if `stack_top == 0`.
    pub fn set_privilege_stack(&mut self, ring: u8, stack_top: u64) -> Result<(), IstError> {
        if ring > 2 {
            return Err(IstError::IndexOutOfRange);
        }
        validate_stack_top(stack_top)?;
        self.tss.privilege_stack[ring as usize] = stack_top;
        Ok(())
    }

    /// Register the top of an IST stack.
    ///
    /// `index` is the 1-based IST number (1..=7) — the same value an
    /// IDT vector encodes in its `ist` field. `stack_top` must be
    /// 16-byte aligned and non-null.
    ///
    /// # Errors
    ///
    /// [`IstError::IndexOutOfRange`] if `index` is not in `1..=7`,
    /// [`IstError::Misaligned`] if `stack_top % 16 != 0`,
    /// [`IstError::NullPointer`] if `stack_top == 0`.
    pub fn set_ist(&mut self, index: u8, stack_top: u64) -> Result<(), IstError> {
        if !(1..=7).contains(&index) {
            return Err(IstError::IndexOutOfRange);
        }
        validate_stack_top(stack_top)?;
        self.tss.ist_stack[(index - 1) as usize] = stack_top;
        Ok(())
    }

    /// Patch the TSS descriptor slots so they point at this
    /// structure's own [`Tss`]. After this call the GDT is ready for
    /// `lgdt`; the structure must not move afterwards.
    pub fn finalize(&mut self) {
        let base = core::ptr::addr_of!(self.tss) as u64;
        // The TSS occupies `size_of::<Tss>()` bytes; the segment limit
        // is "byte length minus one" per SDM Vol 3A §3.4.5. The const
        // assertion `TSS_BYTE_LEN` below pins this to `0x68`, which
        // trivially fits in a `u32`.
        let limit = TSS_BYTE_LEN - 1;
        let [low, high] = tss_descriptor(base, limit, 0);
        self.entries[TSS_INDEX as usize] = low;
        self.entries[TSS_INDEX as usize + 1] = high;
    }

    /// Validate the in-memory table against the privilege-boundary
    /// invariants `install` relies on, **before** `lgdt`/`ltr` load it.
    ///
    /// A corrupted descriptor table is a classic privilege-escalation
    /// vector (call-gate / IST CVE classes): a
    /// kernel CS demoted to DPL 3, a user CS promoted to DPL 0, a
    /// cleared present bit, a TSS descriptor pointing at attacker memory,
    /// or an `RSP0`/IST pivot onto an unmapped stack. Catching these here
    /// makes a scribbled table fail closed instead of being
    /// installed. The check must run after [`Self::finalize`] (it
    /// requires the TSS descriptor to be populated) and after `RSP0` has
    /// been registered via [`Self::set_privilege_stack`].
    ///
    /// # Errors
    ///
    /// A [`GdtError`] naming the first invariant the table breaks.
    pub fn validate(&self) -> Result<(), GdtError> {
        if self.entries[0] != 0 {
            return Err(GdtError::NullSlotNotZero);
        }

        // Kernel code/data: present, code-or-data (S=1), DPL 0.
        for index in [KERNEL_CS_INDEX, KERNEL_DS_INDEX] {
            let d = self.entries[index as usize];
            Self::check_present_user_segment(d, index)?;
            if descriptor_dpl(d) != 0 {
                return Err(GdtError::KernelSegmentNotDpl0 { index });
            }
        }

        // User code/data: present, code-or-data (S=1), DPL 3.
        for index in [USER_CS_INDEX, USER_DS_INDEX] {
            let d = self.entries[index as usize];
            Self::check_present_user_segment(d, index)?;
            if descriptor_dpl(d) != 3 {
                return Err(GdtError::UserSegmentNotDpl3 { index });
            }
        }

        // TSS descriptor: present, system (S=0), available-64-bit-TSS
        // type (0x9), and a base that points at our own TSS.
        let low = self.entries[TSS_INDEX as usize];
        let high = self.entries[TSS_INDEX as usize + 1];
        let present = (low >> 47) & 1 == 1;
        let s_bit = (low >> 44) & 1;
        let ty = (low >> 40) & 0xF;
        if !present || s_bit != 0 || ty != 0x9 {
            return Err(GdtError::TssDescriptorMalformed);
        }
        if tss_base_from_descriptor(low, high) != core::ptr::addr_of!(self.tss) as u64 {
            return Err(GdtError::TssBaseMismatch);
        }

        // `RSP0` is mandatory: a ring 3 → ring 0 transition loads it.
        // Copy the packed array to a local before indexing (E0793).
        let privilege_stack = self.tss.privilege_stack;
        let rsp0 = privilege_stack[0];
        if rsp0 == 0 || !rsp0.is_multiple_of(IST_STACK_ALIGN) {
            return Err(GdtError::PrivilegeStackInvalid);
        }

        // Every *registered* (non-zero) IST stack top must stay aligned.
        let ist = self.tss.ist_stack;
        for (i, &top) in ist.iter().enumerate() {
            if top != 0 && top % IST_STACK_ALIGN != 0 {
                let index = u8::try_from(i + 1).unwrap_or(u8::MAX);
                return Err(GdtError::IstPointerMisaligned { index });
            }
        }

        Ok(())
    }

    /// Shared check: a code/data segment slot must be present (`P=1`)
    /// and a non-system descriptor (`S=1`).
    fn check_present_user_segment(d: u64, index: u16) -> Result<(), GdtError> {
        if (d >> 47) & 1 != 1 {
            return Err(GdtError::SegmentNotPresent { index });
        }
        if (d >> 44) & 1 != 1 {
            return Err(GdtError::NotACodeOrDataSegment { index });
        }
        Ok(())
    }

    /// 16-byte selector tuple for this GDT layout.
    #[must_use]
    pub const fn selectors() -> Selectors {
        Selectors {
            kernel_cs: KERNEL_CS_INDEX << 3,
            kernel_ds: KERNEL_DS_INDEX << 3,
            user_cs: (USER_CS_INDEX << 3) | 3,
            user_ds: (USER_DS_INDEX << 3) | 3,
            tss: TSS_INDEX << 3,
        }
    }

    /// Install this GDT on the current CPU.
    ///
    /// Executes `lgdt`, reloads the data segment registers (DS/ES/FS/
    /// GS/SS) with `kernel_ds`, reloads CS via a far return to a Rust
    /// label, and finally `ltr`s the TSS selector.
    ///
    /// The reference is taken as `&'static mut` to make the lifetime
    /// requirement explicit: `lgdt` stores the *linear* base of the
    /// descriptor table, so the storage must outlive every subsequent
    /// context switch. Taking `&mut` (rather than `&`) reflects the
    /// fact that the CPU writes the TSS busy bit on `ltr` (SDM Vol 3A
    /// §8.2.3), which is a write through the GDT slot.
    ///
    /// # Safety
    ///
    /// * The caller must run this exactly once per CPU.
    /// * The caller must have finalized this `PerCpuGdt` via
    ///   [`Self::finalize`] *and* at least registered IST stacks the
    ///   IDT will reference (if any IDT vector uses a non-zero IST).
    /// * After return, every interrupt / exception will be delivered
    ///   to the kernel through this GDT's CS / TSS. The kernel must
    ///   have an IDT installed (or the trampoline-internal IDT still
    ///   valid) at the time of return.
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    pub unsafe fn install(&'static mut self) {
        // `GDT_SLOTS` is a small compile-time constant, so the byte
        // size of the table fits trivially in a `u16` (the GDT limit
        // field is itself 16 bits). Compute the limit as a `const` to
        // keep the conversion infallible and lint-clean.
        const GDT_LIMIT: u16 = {
            let bytes = size_of::<[u64; GDT_SLOTS]>();
            assert!(bytes >= 1 && bytes - 1 <= u16::MAX as usize);
            // SAFETY-INVARIANT: the `assert!` above proves that
            // `bytes - 1` fits in a `u16`, so the truncation cast
            // is lossless. — justified `#[allow]`.
            #[allow(clippy::cast_possible_truncation)]
            let limit = (bytes - 1) as u16;
            limit
        };
        let gdtr = GdtPointer {
            limit: GDT_LIMIT,
            base: core::ptr::addr_of!(self.entries) as u64,
        };
        let sel = Self::selectors();
        // SAFETY: `gdtr` lives on the local stack for the duration of
        // the `lgdt` instruction, which only reads from the pointer.
        // The GDT itself (`self.entries`) is `'static mut` per the
        // function's safety contract, so the CPU's later references to
        // it through the recorded base address remain valid.
        //
        // Register discipline:
        // * `gdtr` / `kcs` / `kds` / `tss` are read-only `in(reg)`
        //   operands; the assembler picks any free GP register.
        // * `rax` is declared as a clobber once (covering both 16-bit
        //   `ax` and 64-bit `rax` uses below). The `out("ax") _,
        //   out("rax") _` pattern is invalid because the two register
        //   classes overlap.
        unsafe {
            core::arch::asm!(
                "lgdt [{gdtr}]",
                // Reload data segments first.
                "mov ax, {kds:x}",
                "mov ds, ax",
                "mov es, ax",
                "mov fs, ax",
                "mov gs, ax",
                "mov ss, ax",
                // Reload CS via far return.
                "lea rax, [rip + 2f]",
                "push {kcs:r}",
                "push rax",
                "retfq",
                "2:",
                // Load TR last; lgdt does not implicitly touch it.
                "mov ax, {tss:x}",
                "ltr ax",
                gdtr = in(reg) &raw const gdtr,
                kds = in(reg) sel.kernel_ds,
                kcs = in(reg) u64::from(sel.kernel_cs),
                tss = in(reg) sel.tss,
                out("rax") _,
                options(preserves_flags),
            );
        }
    }
}

impl Default for PerCpuGdt {
    fn default() -> Self {
        Self::new()
    }
}

/// Validate a stack-top pointer (non-null + 16-byte aligned).
fn validate_stack_top(stack_top: u64) -> Result<(), IstError> {
    if stack_top == 0 {
        return Err(IstError::NullPointer);
    }
    if !stack_top.is_multiple_of(IST_STACK_ALIGN) {
        return Err(IstError::Misaligned);
    }
    Ok(())
}

// --- Selectors ------------------------------------------------------

/// Segment selectors for the canonical per-CPU GDT layout.
///
/// All values are *encoded* selectors: index in bits 3..16, TI=0 (GDT)
/// in bit 2, RPL in bits 0..2. Kernel selectors are RPL=0; user
/// selectors are RPL=3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selectors {
    /// Kernel code selector — `KERNEL_CS_INDEX << 3`.
    pub kernel_cs: u16,
    /// Kernel data selector — `KERNEL_DS_INDEX << 3`.
    pub kernel_ds: u16,
    /// User code selector — `(USER_CS_INDEX << 3) | 3`.
    pub user_cs: u16,
    /// User data selector — `(USER_DS_INDEX << 3) | 3`.
    pub user_ds: u16,
    /// TSS selector — `TSS_INDEX << 3`.
    pub tss: u16,
}

// --- GDT pseudo-descriptor for `lgdt` -------------------------------

/// 10-byte pseudo-descriptor consumed by `lgdt` / produced by `sgdt`
/// (SDM Vol 3A §3.5.1). Only referenced by [`PerCpuGdt::install`] on
/// `target_os = "none"`.
#[cfg(any(test, all(target_arch = "x86_64", target_os = "none")))]
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct GdtPointer {
    /// Byte length of the GDT minus one.
    pub limit: u16,
    /// Linear base address of the GDT.
    pub base: u64,
}

// --- Tests -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    #[test]
    fn null_descriptor_is_zero() {
        assert_eq!(GdtEntry::NULL.raw(), 0);
    }

    #[test]
    fn kernel_code_descriptor_bits() {
        let raw = GdtEntry::kernel_code().raw();
        // Type=0xA at bits 40..44.
        assert_eq!((raw >> 40) & 0xF, 0xA);
        // S=1 at bit 44.
        assert_eq!((raw >> 44) & 1, 1);
        // DPL=0 at bits 45..47.
        assert_eq!((raw >> 45) & 0x3, 0);
        // P=1 at bit 47.
        assert_eq!((raw >> 47) & 1, 1);
        // L=1 at bit 53.
        assert_eq!((raw >> 53) & 1, 1);
        // D=0 at bit 54 (must be 0 when L=1).
        assert_eq!((raw >> 54) & 1, 0);
    }

    #[test]
    fn kernel_data_descriptor_bits() {
        let raw = GdtEntry::kernel_data().raw();
        assert_eq!((raw >> 40) & 0xF, 0x2);
        assert_eq!((raw >> 44) & 1, 1);
        assert_eq!((raw >> 45) & 0x3, 0);
        assert_eq!((raw >> 47) & 1, 1);
        // L bit irrelevant for data; verify P and DPL only.
    }

    #[test]
    fn user_code_descriptor_has_dpl_three() {
        let raw = GdtEntry::user_code().raw();
        assert_eq!((raw >> 45) & 0x3, 3);
        assert_eq!((raw >> 40) & 0xF, 0xA);
        assert_eq!((raw >> 53) & 1, 1);
    }

    #[test]
    fn user_data_descriptor_has_dpl_three() {
        let raw = GdtEntry::user_data().raw();
        assert_eq!((raw >> 45) & 0x3, 3);
        assert_eq!((raw >> 40) & 0xF, 0x2);
    }

    #[test]
    fn tss_descriptor_splits_base_correctly() {
        // Pick a base that uses every byte so each shift can be
        // verified independently.
        let base: u64 = 0x1234_5678_9ABC_DEF0;
        let limit: u32 = 0x000F_FFFF;
        let [low, high] = tss_descriptor(base, limit, 0);

        // Low slot: limit[0..16] in bits 0..16.
        assert_eq!(low & 0xFFFF, u64::from(limit) & 0xFFFF);
        // Low slot: base[0..24] in bits 16..40.
        assert_eq!((low >> 16) & 0x00FF_FFFF, base & 0x00FF_FFFF);
        // Low slot: type=0x9 at bits 40..44.
        assert_eq!((low >> 40) & 0xF, 0x9);
        // Low slot: S=0 at bit 44.
        assert_eq!((low >> 44) & 1, 0);
        // Low slot: DPL=0 at bits 45..47.
        assert_eq!((low >> 45) & 0x3, 0);
        // Low slot: P=1 at bit 47.
        assert_eq!((low >> 47) & 1, 1);
        // Low slot: limit[16..20] at bits 48..52.
        assert_eq!((low >> 48) & 0xF, u64::from(limit) >> 16);
        // Low slot: base[24..32] at bits 56..64.
        assert_eq!((low >> 56) & 0xFF, (base >> 24) & 0xFF);

        // High slot: base[32..64] in bits 0..32.
        assert_eq!(high & 0xFFFF_FFFF, base >> 32);
        // High slot: reserved upper 32 bits = 0.
        assert_eq!(high >> 32, 0);
    }

    #[test]
    fn tss_descriptor_clamps_dpl_to_two_bits() {
        // Pass DPL=0xFF; should be masked to 0x3 (i.e. DPL=3) and the
        // P bit must remain 1.
        let [low, _] = tss_descriptor(0, 0, 0xFF);
        assert_eq!((low >> 45) & 0x3, 0x3);
        assert_eq!((low >> 47) & 1, 1);
    }

    #[test]
    fn tss_layout_matches_intel_sdm() {
        // SDM Vol 3A §8.7 Figure 8-11:
        //   +0x04  RSP0
        //   +0x0C  RSP1
        //   +0x14  RSP2
        //   +0x24  IST1
        //   +0x2C  IST2
        //   +0x34  IST3
        //   +0x3C  IST4
        //   +0x44  IST5
        //   +0x4C  IST6
        //   +0x54  IST7
        //   +0x66  IOPB
        assert_eq!(offset_of!(Tss, privilege_stack), 0x04);
        assert_eq!(offset_of!(Tss, ist_stack), 0x24);
        assert_eq!(offset_of!(Tss, iopb), 0x66);
        assert_eq!(size_of::<Tss>(), 0x68);
    }

    #[test]
    fn tss_new_zeroes_stack_slots_and_parks_iopb() {
        let tss = Tss::new();
        let privilege_stack = tss.privilege_stack;
        let ist_stack = tss.ist_stack;
        let iopb = tss.iopb;
        assert_eq!(privilege_stack, [0; 3]);
        assert_eq!(ist_stack, [0; 7]);
        assert_eq!(iopb as usize, size_of::<Tss>());
    }

    #[test]
    fn selectors_have_correct_rpl_and_index() {
        let s = PerCpuGdt::selectors();
        assert_eq!(s.kernel_cs, 0x08);
        assert_eq!(s.kernel_ds, 0x10);
        assert_eq!(s.user_ds, (3 << 3) | 3);
        assert_eq!(s.user_cs, (4 << 3) | 3);
        assert_eq!(s.tss, 5 << 3);
    }

    #[test]
    fn new_populates_canonical_segments() {
        let g = PerCpuGdt::new();
        assert_eq!(g.entries[0], 0);
        assert_eq!(
            g.entries[KERNEL_CS_INDEX as usize],
            GdtEntry::kernel_code().raw()
        );
        assert_eq!(
            g.entries[KERNEL_DS_INDEX as usize],
            GdtEntry::kernel_data().raw()
        );
        assert_eq!(
            g.entries[USER_DS_INDEX as usize],
            GdtEntry::user_data().raw()
        );
        assert_eq!(
            g.entries[USER_CS_INDEX as usize],
            GdtEntry::user_code().raw()
        );
        // TSS slots empty before `finalize`.
        assert_eq!(g.entries[TSS_INDEX as usize], 0);
        assert_eq!(g.entries[TSS_INDEX as usize + 1], 0);
    }

    #[test]
    fn finalize_writes_tss_descriptor_for_own_tss() {
        let mut g = PerCpuGdt::new();
        g.finalize();
        let base = core::ptr::addr_of!(g.tss) as u64;
        let limit = TSS_BYTE_LEN - 1;
        let [low, high] = tss_descriptor(base, limit, 0);
        assert_eq!(g.entries[TSS_INDEX as usize], low);
        assert_eq!(g.entries[TSS_INDEX as usize + 1], high);
    }

    #[test]
    fn set_ist_rejects_out_of_range_index() {
        let mut g = PerCpuGdt::new();
        assert_eq!(
            g.set_ist(0, 0x10_0000).unwrap_err(),
            IstError::IndexOutOfRange
        );
        assert_eq!(
            g.set_ist(8, 0x10_0000).unwrap_err(),
            IstError::IndexOutOfRange
        );
    }

    #[test]
    fn set_ist_rejects_misaligned_stack_top() {
        let mut g = PerCpuGdt::new();
        assert_eq!(g.set_ist(1, 0x10_0001).unwrap_err(), IstError::Misaligned);
    }

    #[test]
    fn set_ist_rejects_null_stack_top() {
        let mut g = PerCpuGdt::new();
        assert_eq!(g.set_ist(1, 0).unwrap_err(), IstError::NullPointer);
    }

    #[test]
    fn set_ist_writes_into_correct_tss_slot() {
        let mut g = PerCpuGdt::new();
        g.set_ist(1, 0x0011_0000).unwrap();
        g.set_ist(7, 0x0077_0000).unwrap();
        let ist = g.tss.ist_stack;
        assert_eq!(ist[0], 0x0011_0000);
        assert_eq!(ist[6], 0x0077_0000);
        // Untouched slots remain zero.
        for slot in &ist[1..6] {
            assert_eq!(*slot, 0);
        }
    }

    #[test]
    fn set_privilege_stack_rejects_invalid_ring() {
        let mut g = PerCpuGdt::new();
        assert_eq!(
            g.set_privilege_stack(3, 0x10_0000).unwrap_err(),
            IstError::IndexOutOfRange
        );
    }

    #[test]
    fn set_privilege_stack_writes_into_correct_rsp_slot() {
        let mut g = PerCpuGdt::new();
        g.set_privilege_stack(0, 0x0010_0000).unwrap();
        g.set_privilege_stack(2, 0x0030_0000).unwrap();
        let s = g.tss.privilege_stack;
        assert_eq!(s[0], 0x0010_0000);
        assert_eq!(s[1], 0);
        assert_eq!(s[2], 0x0030_0000);
    }

    #[test]
    fn ist_count_matches_architectural_constant() {
        // The TSS holds exactly 7 IST entries (SDM Vol 3A §8.7).
        assert_eq!(IST_COUNT, 7);
        let tss = Tss::new();
        // Copy to a local to dodge E0793 (reading the length of a
        // packed-struct field requires going through a value, not a
        // borrow).
        let ist = tss.ist_stack;
        assert_eq!(ist.len(), IST_COUNT);
    }

    #[test]
    fn gdt_slots_constant_matches_layout() {
        // 1 null + 4 segments + 2 TSS = 7.
        assert_eq!(GDT_SLOTS, 7);
        let g = PerCpuGdt::new();
        assert_eq!(g.entries.len(), GDT_SLOTS);
    }

    #[test]
    fn gdt_pointer_has_expected_layout() {
        // `lgdt` reads exactly 10 bytes: u16 limit + u64 base.
        assert_eq!(size_of::<GdtPointer>(), 10);
        assert_eq!(offset_of!(GdtPointer, limit), 0);
        assert_eq!(offset_of!(GdtPointer, base), 2);
    }

    // descriptor-table integrity / corruption tests --------------
    //
    // `validate` is the detector for a scribbled GDT/TSS. Each test
    // finalizes a table *in place* (the TSS descriptor embeds the live
    // address of its own TSS, so the struct must not move between
    // `finalize` and `validate`), corrupts one descriptor field through
    // the public `entries` / `tss` storage, and asserts validation fails
    // closed — before any `lgdt`/`ltr`.

    /// Finalize `g` in place and register a valid `RSP0`, leaving a table
    /// that `validate` accepts.
    fn make_valid(g: &mut PerCpuGdt) {
        g.finalize();
        g.set_privilege_stack(0, 0x0010_0000).unwrap();
    }

    #[test]
    fn validate_accepts_a_finalized_table() {
        let mut g = PerCpuGdt::new();
        make_valid(&mut g);
        assert_eq!(g.validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_an_unfinalized_tss_descriptor() {
        let mut g = PerCpuGdt::new();
        g.set_privilege_stack(0, 0x0010_0000).unwrap();
        // No `finalize`: the TSS slot is still zero.
        assert_eq!(g.validate(), Err(GdtError::TssDescriptorMalformed));
    }

    #[test]
    fn validate_requires_a_valid_rsp0() {
        let mut g = PerCpuGdt::new();
        g.finalize();
        // No `RSP0` registered: a ring 3 → ring 0 trap would have no stack.
        assert_eq!(g.validate(), Err(GdtError::PrivilegeStackInvalid));
    }

    #[test]
    fn validate_rejects_a_corrupted_null_slot() {
        let mut g = PerCpuGdt::new();
        make_valid(&mut g);
        g.entries[0] = 0xDEAD_BEEF;
        assert_eq!(g.validate(), Err(GdtError::NullSlotNotZero));
    }

    #[test]
    fn validate_rejects_a_demoted_kernel_cs() {
        let mut g = PerCpuGdt::new();
        make_valid(&mut g);
        // Smear the DPL bits (45..47) of kernel CS to 3 — a privilege
        // downgrade that would expose a kernel selector to ring 3.
        g.entries[KERNEL_CS_INDEX as usize] |= 0x3 << 45;
        assert_eq!(
            g.validate(),
            Err(GdtError::KernelSegmentNotDpl0 {
                index: KERNEL_CS_INDEX
            })
        );
    }

    #[test]
    fn validate_rejects_a_promoted_user_cs() {
        let mut g = PerCpuGdt::new();
        make_valid(&mut g);
        // Clear the DPL bits of user CS to 0 — a privilege escalation.
        g.entries[USER_CS_INDEX as usize] &= !(0x3 << 45);
        assert_eq!(
            g.validate(),
            Err(GdtError::UserSegmentNotDpl3 {
                index: USER_CS_INDEX
            })
        );
    }

    #[test]
    fn validate_rejects_a_not_present_segment() {
        let mut g = PerCpuGdt::new();
        make_valid(&mut g);
        // Clear the present (`P`) bit of kernel DS.
        g.entries[KERNEL_DS_INDEX as usize] &= !(1 << 47);
        assert_eq!(
            g.validate(),
            Err(GdtError::SegmentNotPresent {
                index: KERNEL_DS_INDEX
            })
        );
    }

    #[test]
    fn validate_rejects_a_relocated_tss_base() {
        let mut g = PerCpuGdt::new();
        make_valid(&mut g);
        // Flip a base bit in the TSS descriptor's high word: TR would
        // then point at memory that is not our TSS.
        g.entries[TSS_INDEX as usize + 1] ^= 1;
        assert_eq!(g.validate(), Err(GdtError::TssBaseMismatch));
    }

    #[test]
    fn validate_rejects_a_misaligned_ist_pointer() {
        let mut g = PerCpuGdt::new();
        make_valid(&mut g);
        g.set_ist(1, 0x0011_0000).unwrap();
        // Corrupt the registered IST1 stack top to a misaligned address
        // (direct TSS-metadata tampering, behind the setter's back).
        g.tss.ist_stack[0] = 0x0011_0001;
        assert_eq!(
            g.validate(),
            Err(GdtError::IstPointerMisaligned { index: 1 })
        );
    }
}
