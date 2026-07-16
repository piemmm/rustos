//! AArch64 stage-1 page-table primitives for the memory-isolation test.
//!
//! This module is the aarch64 analogue of `kernel/arch/{x86_64,riscv64}::paging`.
//! It implements the Arch HAL page-table surface
//! ([`rustos_arch_api::mmu::AddressSpace`] +
//! [`rustos_arch_api::tlb::TlbShootdown`]) `kernel/mem` drives: it
//! supplies the architectural mechanism the memory-isolation QEMU
//! vertical needs — two stage-1 translation hierarchies that disagree
//! about a single virtual address, so the MMU faults a process that
//! reaches for another's frame ("memory isolation is
//! enforced by hardware").
//!
//! # Translation scheme
//!
//! 4 KiB granule, three levels (start at L1) covering a 39-bit VA — the
//! aarch64 mirror of riscv64's Sv39, selected by `TCR_EL1.T0SZ = 25`:
//! VA = `L1 (9) | L2 (9) | L3 (9) | offset (12)`. An L1 block descriptor
//! maps 1 GiB, an L2 block 2 MiB, an L3 page 4 KiB (ARM ARM D5.3).
//!
//! Descriptor low bits (ARM ARM D5.3.1): a *table* or *page* descriptor
//! is `0b11`, a *block* descriptor is `0b01`. The lower attributes carry
//! the `MAIR_EL1` attribute index, access permission, shareability, and
//! the access flag.
//!
//! The bit-twiddling that encodes an output address into a descriptor,
//! extracts the per-level table index from a VA, and assembles the
//! attribute words is pure arithmetic and is host-unit-tested below; the
//! `&mut`-recovering table walk and the `TTBR0_EL1`/`SCTLR_EL1` write are
//! gated to the freestanding aarch64 target.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use rustos_arch_api::frames::{reclaim_hierarchy, PageTableFrames, TableFrame};
use rustos_arch_api::mmu::{AddressSpace as MmuAddressSpace, BlockSplit, MapError, PageFlags};
use rustos_arch_api::tlb::TlbShootdown;

/// Size of a single page (and of a page-table page).
pub const PAGE_SIZE: usize = 4096;

/// Size of an L2 block descriptor's translation (2 MiB).
///
/// The granularity [`AddressSpace::split_block`] re-expresses a coarse
/// block down to before reaching 4 KiB leaves, and the alignment the
/// guard-page arena ([`AddressSpace::prepare_guard_arena`]) is laid out
/// at so each guard page becomes its own L3 leaf without disturbing a
/// neighbour.
pub const BLOCK_2MIB: u64 = 2 * 1024 * 1024;

/// Number of 64-bit entries in a stage-1 page-table page.
pub const ENTRIES_PER_TABLE: usize = 512;

/// Number of paging levels (L1 → L2 → L3).
pub const LEVELS: usize = 3;

/// Descriptor low-bit encodings and lower-attribute fields (ARM ARM
/// D5.3.1 / D5.3.3).
pub mod attrs {
    /// Valid bit (bit 0). Set on every live descriptor.
    pub const VALID: u64 = 1 << 0;
    /// Bit 1: set for a table (at L1/L2) or page (at L3) descriptor,
    /// clear for a block descriptor. With [`VALID`] this gives `0b11`
    /// for table/page and `0b01` for block.
    pub const TABLE_OR_PAGE: u64 = 1 << 1;
    /// Access flag (bit 10). Set eagerly so a platform without hardware
    /// AF management does not fault on first touch.
    pub const AF: u64 = 1 << 10;
    /// Inner-shareable (bits `[9:8] = 0b11`).
    pub const SH_INNER: u64 = 0b11 << 8;
    /// Access permission `0b00` (bits `[7:6]`): read/write at EL1, no EL0
    /// access. The kernel-only mapping the isolation test uses.
    pub const AP_RW_EL1: u64 = 0b00 << 6;
    /// Access permission `0b01` (bits `[7:6]`): read/write at EL1 **and**
    /// EL0. Used for an EL0 data mapping (e.g. a user stack).
    pub const AP_RW_EL0: u64 = 0b01 << 6;
    /// Access permission `0b11` (bits `[7:6]`): read-only at EL1 **and**
    /// EL0. Used for an EL0 code mapping (executable but not writable).
    pub const AP_RO_EL0: u64 = 0b11 << 6;
    /// Privileged execute-never (bit 53).
    pub const PXN: u64 = 1 << 53;
    /// Unprivileged execute-never (bit 54).
    pub const UXN: u64 = 1 << 54;
    /// Software-defined leaf bit distinguishing write-combining Normal-NC
    /// framebuffer mappings from bidirectional coherent-DMA mappings.
    pub const SW_WRITE_COMBINE: u64 = 1 << 55;

    /// `MAIR_EL1` attribute index for Normal write-back memory (index 0).
    pub const ATTR_IDX_NORMAL: u64 = 0 << 2;
    /// `MAIR_EL1` attribute index for Device-nGnRE memory (index 1).
    pub const ATTR_IDX_DEVICE: u64 = 1 << 2;
    /// `MAIR_EL1` attribute index for Normal **Non-Cacheable** memory
    /// (index 2). The memory type for a buffer shared with a DMA master
    /// that does not snoop the CPU caches (the BCM2711 PCIe root complex):
    /// the CPU bypasses its caches, so a descriptor it writes is visible to
    /// the device, and an event the device writes is visible to the CPU,
    /// with no explicit cache maintenance. Unlike Device-nGnRE it permits
    /// ordinary (including unaligned) loads/stores, so the xHCI ring and
    /// context structures the driver reads/writes behave normally.
    pub const ATTR_IDX_NORMAL_NC: u64 = 2 << 2;
}

/// `MAIR_EL1` value pairing attribute index 0 = Normal write-back
/// read/write-allocate, index 1 = Device-nGnRE, and index 2 = Normal
/// Non-Cacheable (outer + inner non-cacheable, `0x44`) (ARM ARM D13.2.95).
pub const MAIR_VALUE: u64 = 0xFF | (0x04 << 8) | (0x44 << 16);

/// `TCR_EL1` value for a 39-bit TTBR0 region, 4 KiB granule, inner/outer
/// write-back cacheable, inner-shareable walks, with the upper (TTBR1)
/// half disabled. `T0SZ = 25` ⇒ 39-bit VA (three levels from L1).
pub const TCR_VALUE: u64 = {
    let t0sz: u64 = 25;
    let irgn0: u64 = 0b01 << 8;
    let orgn0: u64 = 0b01 << 10;
    let sh0: u64 = 0b11 << 12;
    let tg0: u64 = 0b00 << 14; // 4 KiB granule for TTBR0
    let epd1: u64 = 1 << 23; // disable TTBR1 walks
    let ips: u64 = 0b010 << 32; // 40-bit (1 TiB) physical address size
    t0sz | irgn0 | orgn0 | sh0 | tg0 | epd1 | ips
};

/// The `SCTLR_EL1` bits that are RES1 on ARMv8.0-A (ARM ARM D13.2.118):
/// bits 29, 28, 23, 22, 20, and 11. Every other bit — including the
/// booby-traps `EE`/`E0E` (data big-endian), `WXN` (writable implies
/// execute-never), `A`/`SA`/`SA0` (alignment checking) — is left clear.
pub const SCTLR_RES1: u64 = (1 << 29) | (1 << 28) | (1 << 23) | (1 << 22) | (1 << 20) | (1 << 11);

/// The known MMU-off `SCTLR_EL1` (= [`SCTLR_RES1`], `0x30D0_0800`) the
/// entry trampolines establish before the first EL1 data access.
///
/// `SCTLR_EL1` is architecturally **UNKNOWN** when EL1 is first entered
/// on real silicon — behind the firmware's EL2 hand-off and behind a
/// PSCI `CPU_ON` alike (QEMU resets it to a benign value, which is why
/// only hardware ever saw the difference). An UNKNOWN `EE` makes every
/// data access byte-swapped; an UNKNOWN `WXN` makes the writable kernel
/// mapping execute-never the instant translation is enabled — a silent
/// pre-vectors hang on the Pi 4. The trampolines (`boot.s` `.Lin_el1`,
/// `smp.s` `_start_secondary_aarch64`) therefore write this exact value
/// — they hard-code `0x30D0_0800`, pinned by a unit test here — so EL1
/// always starts from known ground (: fail closed, not
/// "trust the reset state").
pub const SCTLR_MMU_OFF: u64 = SCTLR_RES1;

/// The full MMU-on `SCTLR_EL1` value `AddressSpace::switch` (freestanding
/// only) installs:
/// [`SCTLR_RES1`] plus `M` (stage-1 translation), `C` (data cache), and
/// `I` (instruction cache).
///
/// Written as a whole — never OR-ed into the live register — so no
/// UNKNOWN reset bit survives into translated execution (see
/// [`SCTLR_MMU_OFF`]). `C` is required, not an optimisation: the
/// LDXR/STXR exclusives the allocator and scheduler rely on are only
/// guaranteed on cacheable Normal memory (a non-cacheable exclusive
/// needs a global monitor the BCM2711 does not provide), and the
/// framebuffer path already cleans its writes to the point of coherency
/// (`crate::video`).
pub const SCTLR_MMU_ON: u64 = SCTLR_RES1 | (1 << 0) | (1 << 2) | (1 << 12);

/// Physical-address mask of a descriptor's output-address field
/// (bits `[47:12]`).
const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;

/// `true` iff a descriptor is a *block* leaf — valid, with bit 1 clear.
#[must_use]
pub const fn is_block(desc: u64) -> bool {
    (desc & attrs::VALID) != 0 && (desc & attrs::TABLE_OR_PAGE) == 0
}

/// Encode an output physical address plus lower attributes into a block
/// or page descriptor. `paddr` must be page/block-aligned.
#[must_use]
pub const fn descriptor(paddr: u64, lower_attrs: u64) -> u64 {
    (paddr & ADDR_MASK) | lower_attrs
}

/// Encode a next-level table pointer into a table descriptor (`0b11`).
#[must_use]
pub const fn table_descriptor(paddr: u64) -> u64 {
    (paddr & ADDR_MASK) | attrs::VALID | attrs::TABLE_OR_PAGE
}

/// Recover the output physical address a descriptor points at.
#[must_use]
pub const fn phys_from_descriptor(desc: u64) -> u64 {
    desc & ADDR_MASK
}

/// Extract the 9-bit table index for paging `level` (1 = top, 3 = leaf)
/// from a virtual address.
#[must_use]
pub const fn table_index(vaddr: u64, level: usize) -> usize {
    // L1 indexes bits [38:30], L2 [29:21], L3 [20:12].
    let shift = 12 + 9 * (LEVELS - level);
    ((vaddr >> shift) & 0x1FF) as usize
}

/// Lower attributes for a kernel Normal-memory leaf (AF, inner
/// shareable, EL1 RW, MAIR index 0), valid block/page.
///
/// The identity-mapped RAM gigapage must remain *privileged-executable*:
/// it backs the kernel's own `.text`, so after the MMU is enabled the
/// next instruction fetch runs from it. `UXN` is set (EL0 must not
/// execute kernel pages) but `PXN` is left clear.
#[must_use]
pub const fn normal_leaf_attrs(block: bool) -> u64 {
    let base = attrs::VALID
        | attrs::AF
        | attrs::SH_INNER
        | attrs::AP_RW_EL1
        | attrs::ATTR_IDX_NORMAL
        | attrs::UXN;
    if block {
        base
    } else {
        base | attrs::TABLE_OR_PAGE
    }
}

/// Lower attributes for an **EL0-executable** Normal-memory page leaf:
/// read-only at EL1 and EL0 (`AP_RO_EL0`), privileged-execute-never
/// (`PXN`, so EL1 cannot run user code) but *unprivileged*-executable
/// (`UXN` clear). The output is a page descriptor (`TABLE_OR_PAGE`); EL0
/// code is always mapped at 4 KiB granularity.
#[must_use]
pub const fn el0_code_leaf_attrs() -> u64 {
    attrs::VALID
        | attrs::TABLE_OR_PAGE
        | attrs::AF
        | attrs::SH_INNER
        | attrs::AP_RO_EL0
        | attrs::ATTR_IDX_NORMAL
        | attrs::PXN
}

/// Lower attributes for an **EL0 read-only, non-executable** Normal-memory
/// page leaf: read-only at EL1 and EL0 (`AP_RO_EL0`) and execute-never at
/// both ELs (`PXN | UXN`). Used for a read-only EL0 data page — an `rxe`
/// `ReadOnly` segment (`.rodata`) or the kernel-written process startup
/// block — where [`el0_code_leaf_attrs`] would wrongly leave the page
/// EL0-executable. The output is a page descriptor (`TABLE_OR_PAGE`).
#[must_use]
pub const fn el0_rodata_leaf_attrs() -> u64 {
    attrs::VALID
        | attrs::TABLE_OR_PAGE
        | attrs::AF
        | attrs::SH_INNER
        | attrs::AP_RO_EL0
        | attrs::ATTR_IDX_NORMAL
        | attrs::PXN
        | attrs::UXN
}

/// Lower attributes for an **EL0-writable** Normal-memory page leaf:
/// read/write at EL1 and EL0 (`AP_RW_EL0`), execute-never at both ELs
/// (`PXN | UXN`). Used for an EL0 data page such as a user stack. The
/// output is a page descriptor (`TABLE_OR_PAGE`).
#[must_use]
pub const fn el0_data_leaf_attrs() -> u64 {
    attrs::VALID
        | attrs::TABLE_OR_PAGE
        | attrs::AF
        | attrs::SH_INNER
        | attrs::AP_RW_EL0
        | attrs::ATTR_IDX_NORMAL
        | attrs::PXN
        | attrs::UXN
}

/// Lower attributes for an **EL0-accessible** Normal **Non-Cacheable**
/// page leaf: read/write at EL1 and EL0 (`AP_RW_EL0`), execute-never at
/// both ELs (`PXN | UXN`), Normal Non-Cacheable memory type (MAIR index
/// 2). Used for a DMA buffer a **user-space driver** shares with a
/// non-I/O-coherent device (the [`PageFlags::DMA_COHERENT`] leaf, the
/// coherent-DMA analogue of [`el0_data_leaf_attrs`]): the buffer is
/// coherent with the device without per-access cache maintenance, and the
/// driver still accesses it with ordinary loads/stores (Device memory
/// would forbid the unaligned ring accesses). The output is a page
/// descriptor (`TABLE_OR_PAGE`); DMA buffers are mapped at 4 KiB
/// granularity.
#[must_use]
pub const fn el0_dma_coherent_leaf_attrs() -> u64 {
    attrs::VALID
        | attrs::TABLE_OR_PAGE
        | attrs::AF
        | attrs::SH_INNER
        | attrs::AP_RW_EL0
        | attrs::ATTR_IDX_NORMAL_NC
        | attrs::PXN
        | attrs::UXN
}

/// Lower attributes for a kernel Device-memory leaf (MAIR index 1,
/// otherwise as [`normal_leaf_attrs`]). Device memory must not be
/// inner-shareable cacheable; the attribute index selects the
/// Device-nGnRE memory type from `MAIR_EL1`.
#[must_use]
pub const fn device_leaf_attrs(block: bool) -> u64 {
    let base = attrs::VALID
        | attrs::AF
        | attrs::AP_RW_EL1
        | attrs::ATTR_IDX_DEVICE
        | attrs::PXN
        | attrs::UXN;
    if block {
        base
    } else {
        base | attrs::TABLE_OR_PAGE
    }
}

/// Lower attributes for an **EL0-accessible** Device-memory page leaf:
/// read/write at EL1 and EL0 (`AP_RW_EL0`), execute-never at both ELs
/// (`PXN | UXN`), Device-nGnRE memory type (MAIR index 1). Used for a
/// device MMIO window a **user-space driver** maps into its own address
/// space through the `mmio_map` syscall (`plans/PI.md` P10 chunk 5d-0):
/// the kernel-only [`device_leaf_attrs`] leaves the page `AP_RW_EL1`, so an
/// EL0 driver reading its own mapped register would take a permission fault
/// — this is the EL0 counterpart, the Device-memory analogue of
/// [`el0_data_leaf_attrs`]. The output is a page descriptor
/// (`TABLE_OR_PAGE`); device windows are always mapped at 4 KiB granularity.
#[must_use]
pub const fn el0_device_leaf_attrs() -> u64 {
    attrs::VALID
        | attrs::TABLE_OR_PAGE
        | attrs::AF
        | attrs::AP_RW_EL0
        | attrs::ATTR_IDX_DEVICE
        | attrs::PXN
        | attrs::UXN
}

/// Number of `u64` words in a gigapage mask covering all
/// [`ENTRIES_PER_TABLE`] L1 slots (one bit per 1 GiB identity gigapage).
pub const GIGAPAGE_MASK_WORDS: usize = ENTRIES_PER_TABLE / 64;

/// Gigapage mask in effect before any board discovery runs: bit 0 only —
/// the QEMU `virt` board keeps its UART, GIC, and the rest of its device
/// MMIO in the first GiB. A board whose MMIO lives elsewhere (the Pi 4's
/// high-peripheral window in gigapage 3) replaces this at boot from its
/// device tree ([`configure_device_gigapages`]); the default is the
/// `virt` value, never a fabricated per-board constant (`plans/PI.md`).
pub const DEFAULT_DEVICE_GIGAPAGES: [u64; GIGAPAGE_MASK_WORDS] = {
    let mut mask = [0u64; GIGAPAGE_MASK_WORDS];
    mask[0] = 1;
    mask
};

/// Identity gigapages currently mapped Device instead of Normal, one bit
/// per L1 slot. Defaults to [`DEFAULT_DEVICE_GIGAPAGES`]; overwritten by
/// [`configure_device_gigapages`] once boot discovery resolves where the
/// board's MMIO actually lives. Read by [`AddressSpace::new_identity_gigapages`]
/// for *every* identity space built after configuration (the boot space
/// and each process space), so the whole system shares one attribute
/// layout.
static DEVICE_GIGAPAGES: [AtomicU64; GIGAPAGE_MASK_WORDS] = [
    AtomicU64::new(DEFAULT_DEVICE_GIGAPAGES[0]),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Install the identity-map Device gigapage mask.
///
/// Called once early in a board's boot path, after device-tree discovery
/// resolves the board's MMIO bases ([`identity_device_mask`]) and before
/// the boot address space is built. `Release` ordering pairs with
/// [`device_gigapages`]' `Acquire` loads so a builder that sees any new
/// word sees a consistent mask.
pub fn configure_device_gigapages(mask: [u64; GIGAPAGE_MASK_WORDS]) {
    for (slot, word) in DEVICE_GIGAPAGES.iter().zip(mask) {
        slot.store(word, Ordering::Release);
    }
}

/// The identity-map Device gigapage mask currently in effect.
#[must_use]
pub fn device_gigapages() -> [u64; GIGAPAGE_MASK_WORDS] {
    let mut mask = [0u64; GIGAPAGE_MASK_WORDS];
    for (word, slot) in mask.iter_mut().zip(&DEVICE_GIGAPAGES) {
        *word = slot.load(Ordering::Acquire);
    }
    mask
}

/// `true` if gigapage `index`'s bit is set in `word` (the mask word
/// covering it) — the one bit test [`gigapage_is_device`] and the
/// constructor's per-word configured-mask read share.
const fn mask_word_bit(word: u64, index: usize) -> bool {
    word & (1 << (index % 64)) != 0
}

/// `true` if identity gigapage `index` is mapped Device under `mask`.
#[must_use]
pub const fn gigapage_is_device(mask: &[u64; GIGAPAGE_MASK_WORDS], index: usize) -> bool {
    index < ENTRIES_PER_TABLE && mask_word_bit(mask[index / 64], index)
}

/// `true` if identity gigapage `index` is mapped Device under the
/// *configured* mask ([`configure_device_gigapages`]), reading exactly
/// the one mask word covering `index`.
///
/// Deliberately scalar: [`AddressSpace::new_identity_gigapages`] runs on
/// boot paths where FP/SIMD may still be trapped (`CPACR_EL1.FPEN`), and
/// copying the whole mask into a 64-byte local is exactly the shape the
/// compiler lowers to vector stores — an EC `0x07` trap with no vectors
/// installed (a silent hang). One `u64` atomic load per query keeps the
/// path integer-only.
fn configured_gigapage_is_device(index: usize) -> bool {
    index < ENTRIES_PER_TABLE
        && mask_word_bit(DEVICE_GIGAPAGES[index / 64].load(Ordering::Acquire), index)
}

/// RAM gigapage mask in effect before any board discovery runs: **all**
/// slots, reproducing the historic "everything not Device is Normal"
/// identity map. Host tests and the QEMU integration kernels build
/// their spaces under this default; a real boot replaces it with the
/// facts in hand ([`configure_ram_gigapages`]) so that gigapages backed
/// by nothing are left *invalid* — on real silicon a Normal write-back
/// executable mapping of unbacked address space invites the core's
/// speculative fetches and prefetches into windows no bus device
/// answers, which can wedge the interconnect the instant translation
/// enables (the metal Pi 4B hung exactly there while QEMU, which
/// answers every address, stayed green).
pub const DEFAULT_RAM_GIGAPAGES: [u64; GIGAPAGE_MASK_WORDS] = [u64::MAX; GIGAPAGE_MASK_WORDS];

/// Identity gigapages currently mapped Normal (RAM), one bit per L1
/// slot. Defaults to [`DEFAULT_RAM_GIGAPAGES`]; overwritten by
/// [`configure_ram_gigapages`] once boot discovery resolves where RAM
/// actually lives. Read by [`AddressSpace::new_identity_gigapages`] for
/// *every* identity space built after configuration, so the whole
/// system shares one attribute layout. A slot in neither this mask nor
/// [`DEVICE_GIGAPAGES`] is left invalid (faults on access — fail
/// closed).
static RAM_GIGAPAGES: [AtomicU64; GIGAPAGE_MASK_WORDS] = [
    AtomicU64::new(u64::MAX),
    AtomicU64::new(u64::MAX),
    AtomicU64::new(u64::MAX),
    AtomicU64::new(u64::MAX),
    AtomicU64::new(u64::MAX),
    AtomicU64::new(u64::MAX),
    AtomicU64::new(u64::MAX),
    AtomicU64::new(u64::MAX),
];

/// Install the identity-map RAM gigapage mask.
///
/// Called on a board's boot path once the RAM-backed extents are known
/// ([`identity_ram_mask`]) and before the boot address space is built;
/// called again when post-MMU discovery widens the known RAM (the
/// firmware `/memory` window), so later-built process spaces map it
/// too. `Release` pairs with the constructor's `Acquire` loads.
pub fn configure_ram_gigapages(mask: [u64; GIGAPAGE_MASK_WORDS]) {
    for (slot, word) in RAM_GIGAPAGES.iter().zip(mask) {
        slot.store(word, Ordering::Release);
    }
}

/// The identity-map RAM gigapage mask currently in effect.
#[must_use]
pub fn ram_gigapages() -> [u64; GIGAPAGE_MASK_WORDS] {
    let mut mask = [0u64; GIGAPAGE_MASK_WORDS];
    for (word, slot) in mask.iter_mut().zip(&RAM_GIGAPAGES) {
        *word = slot.load(Ordering::Acquire);
    }
    mask
}

/// `true` if identity gigapage `index` is mapped Normal (RAM) under the
/// *configured* mask ([`configure_ram_gigapages`]). Scalar — one `u64`
/// atomic load per query — for the same FP/SIMD-trap reason as
/// [`configured_gigapage_is_device`].
fn configured_gigapage_is_ram(index: usize) -> bool {
    index < ENTRIES_PER_TABLE
        && mask_word_bit(RAM_GIGAPAGES[index / 64].load(Ordering::Acquire), index)
}

/// Derive the identity-map RAM gigapage mask from the RAM-backed
/// extents the boot path knows: each `(base, len)` pair marks every
/// gigapage it overlaps. A zero-length extent contributes nothing; an
/// extent reaching past the 512 GiB identity window is clamped (no
/// representable slot beyond it). The caller passes the kernel image's
/// own extent among the inputs, so the executing gigapage is always in
/// the mask — the constructor never builds a space the `switch` caller
/// cannot fetch from.
#[must_use]
pub fn identity_ram_mask(extents: &[(u64, u64)]) -> [u64; GIGAPAGE_MASK_WORDS] {
    let mut mask = [0u64; GIGAPAGE_MASK_WORDS];
    for &(base, len) in extents {
        if len == 0 {
            continue;
        }
        let first = (base >> 30) as usize;
        let last = ((base.saturating_add(len - 1)) >> 30) as usize;
        let mut index = first;
        while index <= last && index < ENTRIES_PER_TABLE {
            mask[index / 64] |= 1 << (index % 64);
            index += 1;
        }
    }
    mask
}

/// Fold one combined (Device | RAM) mask word into a running identity
/// window length: a non-zero word moves the window past its highest set
/// gigapage. The single accumulation [`identity_window_gigapages`] and
/// [`configured_identity_gigapages`] share.
const fn window_fold(window: usize, word_index: usize, combined: u64) -> usize {
    if combined == 0 {
        window
    } else {
        word_index * 64 + (63 - combined.leading_zeros() as usize) + 1
    }
}

/// Number of L1 identity gigapages that covers every gigapage named by
/// either mask: the highest set Device or RAM gigapage plus one, `0`
/// when both masks are empty.
///
/// This is the identity-window length a board-portable caller passes to
/// [`AddressSpace::new_identity_gigapages`] instead of a hard-coded
/// board constant: on the QEMU `virt` board (Device GiB 0, RAM GiB 1)
/// it is 2, on the Pi 4 (RAM from 0, MMIO in GiB 3) it is 4 — a window
/// truncated short of the MMIO gigapage would drop the console and
/// interrupt controller from the space the instant it activates.
#[must_use]
pub fn identity_window_gigapages(
    device: &[u64; GIGAPAGE_MASK_WORDS],
    ram: &[u64; GIGAPAGE_MASK_WORDS],
) -> usize {
    let mut window = 0;
    let mut word_index = 0;
    while word_index < GIGAPAGE_MASK_WORDS {
        window = window_fold(window, word_index, device[word_index] | ram[word_index]);
        word_index += 1;
    }
    window
}

/// [`identity_window_gigapages`] over the *configured* masks
/// ([`configure_device_gigapages`] / [`configure_ram_gigapages`]).
///
/// Deliberately scalar — one atomic `u64` load per mask word, no
/// 64-byte mask local — for the same FP/SIMD-trap reason as
/// `configured_gigapage_is_device`.
#[must_use]
pub fn configured_identity_gigapages() -> usize {
    let mut window = 0;
    let mut word_index = 0;
    while word_index < GIGAPAGE_MASK_WORDS {
        let combined = DEVICE_GIGAPAGES[word_index].load(Ordering::Acquire)
            | RAM_GIGAPAGES[word_index].load(Ordering::Acquire);
        window = window_fold(window, word_index, combined);
        word_index += 1;
    }
    window
}

/// Select the leaf attributes for an identity gigapage from its mask
/// membership: Device wins (MMIO must never be cached or speculated),
/// RAM maps Normal, and a gigapage in neither mask gets **no**
/// descriptor — unbacked address space is left invalid so a stray or
/// speculative access faults instead of wandering onto a bus window
/// nothing answers ([`configure_ram_gigapages`]). The one policy
/// [`AddressSpace::new_identity_gigapages`] applies per slot.
#[must_use]
pub const fn identity_gigapage_leaf(device: bool, ram: bool) -> Option<u64> {
    if device {
        Some(device_leaf_attrs(true))
    } else if ram {
        Some(normal_leaf_attrs(true))
    } else {
        None
    }
}

/// Publish a translation-table store to the MMU's table walker before
/// the next access depends on it: `dsb ishst` orders the store for the
/// walker, `isb` discards any fetch-ahead made under the old tables.
/// Used by [`AddressSpace::ensure_identity_gigapage`]'s invalid→valid
/// L1 update, which needs no TLB invalidation (a walker never caches an
/// invalid entry). Host builds walk no hardware tables, so this is a
/// no-op there.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn publish_table_update() {
    // SAFETY: barrier-only instruction sequence — no memory or register
    // operands, no state observed or mutated beyond ordering.
    unsafe {
        core::arch::asm!("dsb ishst", "isb", options(nostack, preserves_flags));
    }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn publish_table_update() {}

/// True when stage-1 translation is live on this CPU (`SCTLR_EL1.M`).
///
/// [`PageTablePool::alloc`] branches its counter discipline on this:
/// with the MMU off every data access is Device-nGnRnE, where LDXR/STXR
/// exclusives are not architecturally guaranteed to succeed — on the
/// BCM2711 the exclusive monitor never grants them, so an atomic
/// read-modify-write retries forever on real silicon while QEMU's
/// always-granting monitor keeps every emulated boot green.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn translation_enabled() -> bool {
    let sctlr: u64;
    // SAFETY: `SCTLR_EL1` is readable at EL1 and the read has no side
    // effects.
    unsafe {
        core::arch::asm!("mrs {s}, SCTLR_EL1", s = out(reg) sctlr,
            options(nomem, nostack, preserves_flags));
    }
    sctlr & 1 != 0
}

/// Host twin of the `SCTLR_EL1.M` probe: host tests run under a full
/// operating-system memory system where atomic read-modify-writes are
/// always valid, so translation reports live.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn translation_enabled() -> bool {
    true
}

/// Clean+invalidate every data-cache line of `[base, base + len)` to the
/// point of coherency (`dc civac`, line size decoded from the live
/// `CTR_EL0` by [`dcache_line_bytes`]), then `dsb sy`.
///
/// This is the bridge between the boot CPU's cacheable writes and an
/// observer that reads the same bytes non-cacheably — the translation
/// walker before the MMU enables, or a freshly-started secondary core
/// running MMU-off: without the sweep, a dirty (or stale) line over the
/// range would shadow — or later overwrite — the DRAM bytes the
/// non-cacheable observer works with on real silicon (cache-less QEMU
/// cannot show it).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn clean_invalidate_range_to_poc(base: u64, len: u64) {
    if len == 0 {
        return;
    }
    let ctr: u64;
    // SAFETY: `CTR_EL0` is an unprivileged read-only identification
    // register; reading it has no side effects.
    unsafe {
        core::arch::asm!("mrs {ctr}, CTR_EL0", ctr = out(reg) ctr,
            options(nomem, nostack, preserves_flags));
    }
    let line = dcache_line_bytes(ctr);
    // Sweep from the line-aligned base so the first partial line is
    // covered too.
    let mut addr = base & !(line - 1);
    let end = base.saturating_add(len);
    while addr < end {
        // SAFETY: `dc civac` performs cache maintenance only — it never
        // changes memory contents — so it is sound for any address; the
        // caller names a range it owns.
        unsafe {
            core::arch::asm!("dc civac, {addr}", addr = in(reg) addr,
                options(nostack, preserves_flags));
        }
        addr += line;
    }
    // SAFETY: barrier-only instruction — completes the maintenance in
    // the full-system domain before the non-cacheable observer reads.
    unsafe {
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}

/// Host stand-in for [`clean_invalidate_range_to_poc`]: the host has no
/// data cache to maintain, so the sweep is vacuous.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
pub fn clean_invalidate_range_to_poc(_base: u64, _len: u64) {}

/// Smallest data-cache line size in bytes encoded by a `CTR_EL0` value:
/// `DminLine` (bits `[19:16]`) is the log2 of that line's length in
/// *words*, so the byte length is `4 << DminLine` (ARM ARM D13.2.34 —
/// 64 bytes on the Cortex-A72's `0x8444_C004`). Pure so the decode is
/// host-unit-tested; the freestanding cache-maintenance sweep
/// ([`clean_invalidate_range_to_poc`]) feeds it the live register.
#[must_use]
pub const fn dcache_line_bytes(ctr_el0: u64) -> u64 {
    4 << ((ctr_el0 >> 16) & 0xF)
}

/// Derive the identity-map Device gigapage mask from the board's
/// discovered MMIO bases and the kernel image's own extent.
///
/// Each gigapage containing one of `device_bases` is mapped Device so
/// MMIO reads/writes are not cached, reordered, or speculated
/// (Device-nGnRE is the only correct attribute for a
/// register block). The gigapages overlapping `[kernel_start,
/// kernel_end)` are forced Normal regardless: the CPU executes the
/// kernel image out of them, and a Device(+PXN) mapping would fault the
/// instruction fetch the moment the MMU comes on — on the Pi 4 the
/// kernel at `0x8_0000` shares gigapage 0 with nothing the kernel
/// drives, while its UART/GIC live in gigapage 3 (`plans/PI.md` §1). On
/// QEMU `virt` the kernel sits in gigapage 1 and the MMIO in gigapage
/// 0, reproducing the historic layout. A base beyond the 512 GiB
/// identity window is ignored (no representable slot).
#[must_use]
pub fn identity_device_mask(
    device_bases: &[u64],
    kernel_start: u64,
    kernel_end: u64,
) -> [u64; GIGAPAGE_MASK_WORDS] {
    let mut mask = [0u64; GIGAPAGE_MASK_WORDS];
    for &base in device_bases {
        let index = (base >> 30) as usize;
        if index < ENTRIES_PER_TABLE {
            mask[index / 64] |= 1 << (index % 64);
        }
    }
    // The kernel image's gigapages stay Normal — executable — even if a
    // discovered MMIO base lands in one (the conflict is unmappable at
    // 1 GiB granularity; keeping the CPU running wins).
    let first = (kernel_start >> 30) as usize;
    let last_byte = if kernel_end > kernel_start {
        kernel_end - 1
    } else {
        kernel_start
    };
    let last = (last_byte >> 30) as usize;
    let mut index = first;
    while index <= last && index < ENTRIES_PER_TABLE {
        mask[index / 64] &= !(1 << (index % 64));
        index += 1;
    }
    mask
}

/// One page-table page: 512 × u64, naturally aligned.
#[repr(C, align(4096))]
struct Table([u64; ENTRIES_PER_TABLE]);

impl Table {
    const fn new() -> Self {
        Self([0; ENTRIES_PER_TABLE])
    }
}

/// Default pool capacity: what the memory-isolation test and the small
/// bootstrap spaces need — two [`AddressSpace`]s, each a root plus a
/// 3-level walk for the extra 4 KiB mapping, with spares. A consumer with
/// a larger, *derived* demand (the boot pool that also re-expresses the
/// kthread-stack guard arena at 4 KiB granularity) instantiates
/// [`PageTablePool`] with its own capacity instead of growing this
/// default for everyone.
const POOL_SIZE: usize = 16;

/// Page-table frames a boot pool must hold to build the gigapage identity
/// map *and* re-express a guard arena of `arena_bytes` at 4 KiB granularity
/// ([`AddressSpace::prepare_guard_arena`]): one L1 root, up to two L2
/// replacement tables (an arena can straddle a 1 GiB boundary, splitting
/// two gigapage blocks), and one L3 replacement table per 2 MiB block the
/// arena spans.
///
/// The capacity is derived from the arena the caller intends to prepare,
/// never a hand-picked literal: a fixed default-sized boot pool passed
/// QEMU `virt` (whose small RAM window sizes a small arena) but exhausted
/// mid-split on a real 8 GiB Pi 4 (64 MiB arena = 33+ tables), silently
/// degrading the kthread stacks to their software-canary form.
#[must_use]
pub const fn guard_arena_pool_capacity(arena_bytes: u64) -> usize {
    1 + 2 + (arena_bytes / BLOCK_2MIB) as usize
}

/// A statically-allocated pool of zero-initialised page-table pages.
///
/// Allocation is monotonic — frames are never freed — which matches the
/// set-up → run → exit lifecycle of the isolation test. A real allocator
/// lives in `kernel/mem` and is wired in by a later stage.
pub struct PageTablePool<const CAPACITY: usize = POOL_SIZE> {
    storage: [UnsafeCell<Table>; CAPACITY],
    used: AtomicUsize,
}

// SAFETY: the pool exposes `&self` allocation but every allocated frame
// is handed out exactly once — the counter is a monotonic `AtomicUsize`
// advanced by `fetch_add` whenever translation is live, and by the
// single-threaded pre-SMP boot CPU alone when it is not
// ([`PageTablePool::alloc_with`]) — so distinct allocations never alias.
unsafe impl<const CAPACITY: usize> Sync for PageTablePool<CAPACITY> {}

impl<const CAPACITY: usize> Default for PageTablePool<CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const CAPACITY: usize> PageTablePool<CAPACITY> {
    /// Construct an empty pool. `const`, so the pool lives in `.bss`.
    #[must_use]
    pub const fn new() -> Self {
        #[allow(clippy::declare_interior_mutable_const)]
        const ZERO: UnsafeCell<Table> = UnsafeCell::new(Table::new());
        #[allow(clippy::large_stack_arrays)]
        let storage = [ZERO; CAPACITY];
        Self {
            storage,
            used: AtomicUsize::new(0),
        }
    }

    /// Allocate a fresh, zero-initialised table page.
    ///
    /// Returns `None` when the pool is exhausted — callers handle it as
    /// a closed-fail (: deterministic OOM, never panic).
    pub fn alloc(&self) -> Option<&'static mut [u64; ENTRIES_PER_TABLE]> {
        self.alloc_with(translation_enabled())
    }

    /// [`Self::alloc`] with the translation state passed in, so the
    /// host unit tests can exercise both counter disciplines.
    ///
    /// With translation live the monotonic counter advances by atomic
    /// `fetch_add` — the pool is shared (`Sync`) and a concurrent
    /// allocator on another CPU must observe a unique index. With
    /// translation *off* that very `fetch_add` is the defect: its
    /// LDXR/STXR exclusives target Device-nGnRnE memory, where the
    /// BCM2711 never grants the exclusive monitor, so the retry loop
    /// spins forever on real silicon (QEMU's monitor always succeeds,
    /// which kept every emulated boot green). The MMU-off discipline is
    /// therefore a plain load + store — and that is sound because
    /// MMU-off allocation is single-threaded by construction: only the
    /// pre-SMP boot CPU runs Rust with translation disabled and a pool
    /// in hand (a secondary core allocates nothing before it switches
    /// to the already-built boot space).
    fn alloc_with(&self, translation_live: bool) -> Option<&'static mut [u64; ENTRIES_PER_TABLE]> {
        let idx = if translation_live {
            let idx = self.used.fetch_add(1, Ordering::SeqCst);
            if idx >= CAPACITY {
                // Park the counter at the cap so a pathological number
                // of post-exhaustion calls cannot wrap it.
                self.used.store(CAPACITY, Ordering::SeqCst);
            }
            idx
        } else {
            let idx = self.used.load(Ordering::SeqCst);
            if idx < CAPACITY {
                self.used.store(idx + 1, Ordering::SeqCst);
            }
            idx
        };
        if idx >= CAPACITY {
            return None;
        }
        // SAFETY: the monotonic counter means this index is owned by
        // *this* call uniquely — via atomic `fetch_add` when translation
        // is live, and via the single-threaded pre-SMP boot-CPU
        // invariant documented above when it is not — so the returned
        // `&'static mut` never aliases another.
        let cell = &self.storage[idx];
        let table_ref: &'static mut Table = unsafe { &mut *cell.get() };
        Some(&mut table_ref.0)
    }

    /// Clean+invalidate every data-cache line of the pool's backing
    /// storage to the point of coherency (`dc civac`, line size decoded
    /// from the live `CTR_EL0` by [`dcache_line_bytes`]).
    ///
    /// The boot path calls this once, after the identity tables are
    /// written and before `AddressSpace::switch` enables the MMU: the
    /// tables were written with the data cache **off** (every MMU-off
    /// store is Device-nGnRnE, straight to DRAM), but the walker reads
    /// them back *cacheable* (`TCR_VALUE` IRGN0/ORGN0) the instant
    /// translation enables — any stale line the firmware left over the
    /// pool's addresses would then shadow the real descriptors on real
    /// silicon (cache-less QEMU cannot show it). The same residue
    /// hazard is why Linux's `head.S` invalidates its idmap tables to
    /// PoC before `__enable_mmu`. Fail-closed: the whole fixed-size
    /// pool is swept (one pass over 64 KiB at boot — off every hot
    /// path), not just the slots handed out so far.
    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
    pub fn clean_invalidate_to_poc(&self) {
        clean_invalidate_range_to_poc(
            self.storage.as_ptr() as u64,
            core::mem::size_of_val(&self.storage) as u64,
        );
    }

    /// Host twin of the freestanding clean+invalidate: host builds have
    /// no hardware cache to maintain, so this is a no-op (mirrors
    /// `publish_table_update`).
    #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
    pub fn clean_invalidate_to_poc(&self) {}
}

impl<const CAPACITY: usize> PageTableFrames for PageTablePool<CAPACITY> {
    fn alloc_table(&self) -> Option<TableFrame> {
        let entries = self.alloc()?;
        // The kernel's own memory is identity-mapped (MMU-off boot then a
        // gigapage identity map), so a table's virtual address is its
        // physical address (`plans/WIRING.md` W5b-3 —
        // the bootstrap frame source).
        let phys = phys_of(entries);
        Some(TableFrame { phys, entries })
    }

    fn free_table(&self, phys: u64) {
        // The boot pool is a bump allocator over permanent kernel-image
        // `.bss`: its storage is never reclaimable RAM and the boot space
        // built over it is never torn down, so a returned frame is retired
        // without reuse. Per-process spaces draw from the allocator-backed
        // `kernel/mem` source, whose `free_table` genuinely recycles.
        let _ = phys;
    }
}

/// A stage-1 address space built on a freshly-allocated L1 root table.
///
/// The constructor identity-maps the low `gigabytes` GiB of physical
/// memory with 1 GiB L1 block descriptors so the kernel's own
/// code/stack/data and the board's MMIO remain reachable whichever
/// [`AddressSpace`] is active. The gigapages named by the configured
/// Device mask ([`configure_device_gigapages`] — by default gigapage 0,
/// which holds the `virt` board's PL011 UART and GIC) are mapped
/// Device; the rest Normal. [`Self::map_4k`] adds the finer-grained
/// mappings the memory-isolation test diverges on.
pub struct AddressSpace {
    root_phys: u64,
    root: &'static mut [u64; ENTRIES_PER_TABLE],
    /// The frame source the page-table walk allocates intermediate
    /// tables from, retained so the [`rustos_arch_api::mmu::AddressSpace`]
    /// HAL impl can install mappings without the caller re-supplying it.
    /// The static [`PageTablePool`] is the boot/bootstrap source; a real
    /// per-process space is built over the `kernel/mem` frame-allocator
    /// source (`plans/WIRING.md` W5b-3).
    frames: &'static dyn PageTableFrames,
}

impl AddressSpace {
    /// Build a new address space identity-mapping `[0, gigabytes GiB)`
    /// with 1 GiB L1 block descriptors.
    ///
    /// `gigabytes` must be `1..=512` (the number of L1 slots). On the
    /// QEMU `virt` board two gigapages cover the device MMIO window and
    /// the RAM base at `0x4000_0000`.
    ///
    /// # Errors
    ///
    /// Returns `None` if `gigabytes` is out of range or the page-table
    /// pool is exhausted.
    pub fn new_identity_gigapages(
        frames: &'static dyn PageTableFrames,
        gigabytes: usize,
    ) -> Option<Self> {
        if gigabytes == 0 || gigabytes > ENTRIES_PER_TABLE {
            return None;
        }
        let TableFrame {
            phys: root_phys,
            entries: root,
        } = frames.alloc_table()?;
        // The board-configured Device mask says which gigapages hold MMIO
        // (`virt`: GiB 0; Pi 4: GiB 3); the RAM mask says which hold
        // RAM-backed memory. A slot in neither mask stays *invalid*:
        // unbacked address space must fault, never invite speculation
        // ([`RAM_GIGAPAGES`]). The masks are read one word per slot
        // (`configured_gigapage_is_device` /
        // `configured_gigapage_is_ram`) so the constructor stays
        // FP/SIMD-free — it runs before some callers enable
        // `CPACR_EL1.FPEN`.
        for (i, slot) in root.iter_mut().take(gigabytes).enumerate() {
            let paddr = (i as u64) << 30;
            let Some(leaf) = identity_gigapage_leaf(
                configured_gigapage_is_device(i),
                configured_gigapage_is_ram(i),
            ) else {
                continue;
            };
            *slot = descriptor(paddr, leaf);
        }
        Some(Self {
            root_phys,
            root,
            frames,
        })
    }

    /// Install the identity gigapage containing `paddr` into this live
    /// space when its L1 slot is still invalid, choosing the same leaf
    /// the constructor would (Device per the configured mask, else
    /// Normal), and publish the table write to the walker.
    ///
    /// The boot path calls this after the post-MMU `/memory` discovery
    /// widens the known RAM beyond the pre-MMU
    /// [`configure_ram_gigapages`] facts: an invalid→valid L1 update
    /// needs no TLB invalidation (a walker never caches an invalid
    /// entry), only a store barrier before the next access. Returns
    /// `false` — fail closed, nothing written — when `paddr` lies
    /// beyond the identity window; an already-valid slot is left
    /// untouched and reported `true`.
    pub fn ensure_identity_gigapage(&mut self, paddr: u64) -> bool {
        let index = (paddr >> 30) as usize;
        if index >= ENTRIES_PER_TABLE {
            return false;
        }
        if (self.root[index] & attrs::VALID) != 0 {
            return true;
        }
        let leaf = if configured_gigapage_is_device(index) {
            device_leaf_attrs(true)
        } else {
            normal_leaf_attrs(true)
        };
        self.root[index] = descriptor((index as u64) << 30, leaf);
        publish_table_update();
        true
    }

    /// `true` if `vaddr` already resolves to a live leaf (block or page)
    /// in this hierarchy.
    ///
    /// A read-only stage-1 walk used by the
    /// [`rustos_arch_api::mmu::AddressSpace`] HAL impl to report
    /// [`rustos_arch_api::mmu::MapError::AlreadyMapped`] rather than
    /// silently clobber an existing mapping. It dereferences present
    /// table descriptors through the identity map (phys == virt for every
    /// table the kernel owns), the same round-trip [`ensure_child`] uses.
    fn leaf_present(&self, vaddr: u64) -> bool {
        let e1 = self.root[table_index(vaddr, 1)];
        if (e1 & attrs::VALID) == 0 {
            return false;
        }
        if is_block(e1) {
            return true;
        }
        // SAFETY: a present table descriptor holds an output address
        // `ensure_child` wrote from `phys_of(&mut [u64; 512])`; identity
        // mapping makes it dereferenceable directly.
        let l2 = unsafe { &*(phys_from_descriptor(e1) as *const [u64; ENTRIES_PER_TABLE]) };
        let e2 = l2[table_index(vaddr, 2)];
        if (e2 & attrs::VALID) == 0 {
            return false;
        }
        if is_block(e2) {
            return true;
        }
        // SAFETY: as above — a present L2 table descriptor's output
        // address is a valid identity-mapped table.
        let l3 = unsafe { &*(phys_from_descriptor(e2) as *const [u64; ENTRIES_PER_TABLE]) };
        (l3[table_index(vaddr, 3)] & attrs::VALID) != 0
    }

    /// Map `paddr` at `vaddr` with 4 KiB granularity as Normal memory
    /// (kernel-only, EL1 RW, execute-never at EL0).
    ///
    /// `vaddr` and `paddr` must be page-aligned. Returns `None` on
    /// page-table-pool exhaustion or if the walk meets an existing block
    /// it would have to shatter — the isolation test maps outside the
    /// identity-mapped gigapages so that path is not exercised.
    pub fn map_4k(
        &mut self,
        frames: &'static dyn PageTableFrames,
        vaddr: u64,
        paddr: u64,
    ) -> Option<()> {
        self.map_4k_with_attrs(frames, vaddr, paddr, normal_leaf_attrs(false))
    }

    /// Map `paddr` at `vaddr` with 4 KiB granularity using the supplied
    /// page-leaf `leaf_attrs` (e.g. [`el0_code_leaf_attrs`] /
    /// [`el0_data_leaf_attrs`] for an EL0 user mapping). `map_4k` is this
    /// with the kernel-only [`normal_leaf_attrs`], so there is one walk
    /// implementation.
    ///
    /// `vaddr` and `paddr` must be page-aligned. Returns `None` on
    /// page-table-pool exhaustion or if the walk meets an existing block
    /// it would have to shatter.
    pub fn map_4k_with_attrs(
        &mut self,
        frames: &'static dyn PageTableFrames,
        vaddr: u64,
        paddr: u64,
        leaf_attrs: u64,
    ) -> Option<()> {
        if (vaddr & (PAGE_SIZE as u64 - 1)) != 0 || (paddr & (PAGE_SIZE as u64 - 1)) != 0 {
            return None;
        }
        let i1 = table_index(vaddr, 1);
        let i2 = table_index(vaddr, 2);
        let i3 = table_index(vaddr, 3);

        let l2 = ensure_child(self.root, i1, frames)?;
        let l3 = ensure_child(l2, i2, frames)?;
        if (l3[i3] & attrs::VALID) != 0 {
            return None;
        }
        l3[i3] = descriptor(paddr, leaf_attrs);
        Some(())
    }

    /// Split the coarse block descriptor(s) covering `vaddr` down to
    /// 4 KiB granularity, preserving the mapped output address and every
    /// attribute, so the single 4 KiB page containing `vaddr` can then be
    /// torn down with [`MmuAddressSpace::unmap`] without disturbing its
    /// neighbours.
    ///
    /// This is the foundation of the kthread guard page (`plans/PI.md`):
    /// a guard page that falls inside a region the boot path mapped with
    /// coarse 1 GiB / 2 MiB *block* descriptors cannot be unmapped while
    /// it is part of a block, because a block has no per-4 KiB leaf to
    /// clear. Splitting re-expresses the same translation as a table of
    /// finer descriptors — an L1 block (1 GiB) becomes a table of 512 ×
    /// 2 MiB blocks, then the 2 MiB block covering `vaddr` becomes a table
    /// of 512 × 4 KiB pages — leaving every address translating
    /// identically but now at 4 KiB granularity.
    ///
    /// The split is **break-before-make-free for the running region**: it
    /// only ever *adds* table levels that reproduce the existing
    /// translation, never invalidating a live address, so it is safe to
    /// run against the active translation regime (the resulting tables map
    /// the same physical frames with the same permissions). It is
    /// idempotent — a level that is already a table is left untouched — so
    /// re-splitting an already-fine region succeeds without allocating.
    /// The caller is responsible for any TLB maintenance after a
    /// subsequent [`MmuAddressSpace::unmap`]; the split itself changes no
    /// translation result and so needs none.
    ///
    /// # Errors
    ///
    /// Returns [`MapError::Misaligned`] if `vaddr` is not 4 KiB-aligned,
    /// [`MapError::NotMapped`] if `vaddr` has no live mapping at the level
    /// being split (nothing to shatter), or [`MapError::PoolExhausted`] if
    /// the page-table pool cannot supply a replacement table. On
    /// [`MapError::PoolExhausted`] any level already split stays split
    /// (still a faithful identity re-expression of the same translation),
    /// so the address space is never left describing a *different*
    /// mapping (fail closed, never corrupt).
    pub fn split_block(&mut self, vaddr: u64) -> Result<(), MapError> {
        if (vaddr & (PAGE_SIZE as u64 - 1)) != 0 {
            return Err(MapError::Misaligned);
        }
        let frames = self.frames;
        let i1 = table_index(vaddr, 1);

        // --- Level 1: a 1 GiB block becomes a table of 512 × 2 MiB blocks.
        let e1 = self.root[i1];
        if (e1 & attrs::VALID) == 0 {
            return Err(MapError::NotMapped);
        }
        if is_block(e1) {
            let TableFrame { phys, entries } =
                frames.alloc_table().ok_or(MapError::PoolExhausted)?;
            // 2 MiB sub-entries (shift 21) are still *blocks*, not pages.
            shatter_block_into(entries, e1, 21, false);
            self.root[i1] = table_descriptor(phys);
        }

        // L1 now holds a table descriptor; recover the L2 table it points at.
        // SAFETY: the entry is a present, non-block table descriptor (just
        // installed above, or pre-existing); its output address is an
        // identity-mapped table page (the round-trip `ensure_child` relies
        // on), and `&mut self` makes the borrow exclusive.
        let l2 =
            unsafe { &mut *(phys_from_descriptor(self.root[i1]) as *mut [u64; ENTRIES_PER_TABLE]) };
        let i2 = table_index(vaddr, 2);
        let e2 = l2[i2];
        if (e2 & attrs::VALID) == 0 {
            return Err(MapError::NotMapped);
        }
        if is_block(e2) {
            let TableFrame { phys, entries } =
                frames.alloc_table().ok_or(MapError::PoolExhausted)?;
            // 4 KiB sub-entries (shift 12) are L3 *pages* (`TABLE_OR_PAGE`).
            shatter_block_into(entries, e2, 12, true);
            l2[i2] = table_descriptor(phys);
        }
        // L2 now resolves `vaddr` through a 4 KiB page leaf.
        Ok(())
    }

    /// Re-express every coarse block covering the arena
    /// `[base, base + len)` at 4 KiB granularity, so any single page in
    /// the arena can later be unmapped (e.g. a kthread kernel-stack guard
    /// page) without shattering the block the running CPU executes on
    /// (`plans/PI.md` guard-page fault-form, stage G2).
    ///
    /// This is [`Self::split_block`] applied to every 2 MiB block the
    /// arena spans: a guard-page arena that the boot path laid down inside
    /// the coarse identity gigapages has no per-4 KiB leaf to clear, so the
    /// whole arena is split up-front, at boot, while it holds no running
    /// context. Because `split_block` only ever *adds* table levels that
    /// reproduce the existing translation, preparing the arena changes no
    /// address's mapping and needs no TLB maintenance — it is safe against
    /// the active translation regime and is idempotent (a re-prepare of an
    /// already-fine arena allocates nothing).
    ///
    /// `base` and `len` are taken in bytes; `base` must be 4 KiB-aligned
    /// (the arena is laid out 2 MiB-aligned, which satisfies this). The
    /// arena is walked from the 2 MiB block containing `base` through the
    /// block containing its last byte, so an arena that is not itself a
    /// whole number of 2 MiB blocks still has every covering block split.
    ///
    /// # Errors
    ///
    /// Returns [`MapError::Misaligned`] if `len` is zero or `base` is not
    /// 4 KiB-aligned, [`MapError::NotMapped`] if any covering block has no
    /// live mapping, or [`MapError::PoolExhausted`] if the page-table pool
    /// cannot supply a replacement table. On a mid-arena failure the
    /// blocks already split stay split (a faithful re-expression of the
    /// same translation), so the space never describes a *different*
    /// mapping (fail closed, never corrupt).
    pub fn prepare_guard_arena(&mut self, base: u64, len: u64) -> Result<(), MapError> {
        if len == 0 || (base & (PAGE_SIZE as u64 - 1)) != 0 {
            return Err(MapError::Misaligned);
        }
        // The last byte the arena occupies; `len != 0`, so `base + len`
        // does not underflow when computing it. A `base + len` that wraps
        // `u64` is rejected as a fail-closed `Misaligned` (a degenerate
        // arena), never silently truncated.
        let last = base.checked_add(len - 1).ok_or(MapError::Misaligned)?;
        let first_block = base & !(BLOCK_2MIB - 1);
        let last_block = last & !(BLOCK_2MIB - 1);
        let mut block = first_block;
        loop {
            self.split_block(block)?;
            if block == last_block {
                break;
            }
            block += BLOCK_2MIB;
        }
        Ok(())
    }

    /// Physical address of the L1 root table (the value programmed into
    /// `TTBR0_EL1`). Exposed so tests can observe it.
    #[must_use]
    pub fn root_phys(&self) -> u64 {
        self.root_phys
    }

    /// Translate the architecture-neutral [`PageFlags`] into a stage-1
    /// page-leaf attribute word (one neutral
    /// vocabulary, decoded once at the HAL boundary). W^X is the default: an executable user page is mapped read-only
    /// ([`el0_code_leaf_attrs`]); a writable user page is execute-never
    /// ([`el0_data_leaf_attrs`]); a read-only user page is execute-never
    /// ([`el0_rodata_leaf_attrs`]). A kernel page uses the EL1 RW,
    /// EL0-execute-never [`normal_leaf_attrs`]; a kernel Device page uses
    /// [`device_leaf_attrs`], and an **EL0** Device page (a user-space
    /// driver's `mmio_map` window, `DEVICE | USER`) uses the EL0-accessible
    /// [`el0_device_leaf_attrs`] — otherwise the driver would take a
    /// permission fault reading its own mapped register (`plans/PI.md` P10
    /// chunk 5d-0).
    fn leaf_attrs_for(flags: PageFlags) -> u64 {
        if flags.contains(PageFlags::DEVICE) {
            if flags.contains(PageFlags::USER) {
                el0_device_leaf_attrs()
            } else {
                device_leaf_attrs(false)
            }
        } else if flags.contains(PageFlags::WRITE_COMBINE) {
            el0_dma_coherent_leaf_attrs() | attrs::SW_WRITE_COMBINE
        } else if flags.contains(PageFlags::DMA_COHERENT) {
            // A buffer shared with a non-I/O-coherent DMA master (the
            // BCM2711 PCIe root complex): Normal Non-Cacheable so the device
            // and CPU see each other's writes without cache maintenance,
            // while ordinary ring/context loads/stores still work (Device
            // memory would forbid them). Always EL0-accessible RW,
            // execute-never — the only consumer is a user-space driver's DMA
            // carve.
            el0_dma_coherent_leaf_attrs()
        } else if flags.contains(PageFlags::USER) {
            if flags.contains(PageFlags::EXEC) {
                el0_code_leaf_attrs()
            } else if flags.contains(PageFlags::WRITE) {
                el0_data_leaf_attrs()
            } else {
                el0_rodata_leaf_attrs()
            }
        } else {
            normal_leaf_attrs(false)
        }
    }

    /// Activate this address space: program `MAIR_EL1`, `TCR_EL1`,
    /// `TTBR0_EL1`, and install the full known [`SCTLR_MMU_ON`] value
    /// (translation plus caches), then synchronise.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that this address space identity-maps
    /// the currently-executing `pc`, the current stack, and every MMIO
    /// region the code touches before the next `switch` — otherwise the
    /// CPU faults on the next fetch/access.
    /// [`Self::new_identity_gigapages`] upholds that by identity-mapping
    /// the kernel's gigapages (RAM Normal, MMIO Device per the configured
    /// [`configure_device_gigapages`] mask).
    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
    pub unsafe fn switch(&self) {
        // SAFETY: the caller asserts the new mappings cover `pc`, `sp`,
        // and MMIO. Programming MAIR/TCR/TTBR0 then writing `SCTLR_EL1`
        // is the documented stage-1 enable sequence; the first barrier is
        // `dsb sy` — not `ish` — because the translation tables were
        // written with the MMU off, where every store is Device-nGnRnE
        // and therefore ordered in the *full-system* domain (an
        // inner-shareable barrier is not architecturally guaranteed to
        // order them ahead of the walker's first cacheable read; QEMU
        // cannot show the difference). The `tlbi vmalle1`
        // + `dsb`/`isb` flush stale translations, `ic iallu` starts the
        // instruction cache invalid before [`SCTLR_MMU_ON`] enables it,
        // and the *whole-register* write installs a fully known value —
        // an OR of `M` into the live register would carry the
        // architecturally UNKNOWN EL1 reset bits (`WXN`, `EE`, …) into
        // translated execution, which hangs real silicon (see
        // [`SCTLR_MMU_OFF`]).
        // The first fully-configured space activated on the metal is the
        // permanent boot space: publish its root, set-once, as the park
        // root teardown and the dispatcher's suspend path re-install so a
        // dead user root is never left active (see [`park_kernel_root`]).
        let _ = PARK_ROOT.compare_exchange(0, self.root_phys, Ordering::AcqRel, Ordering::Relaxed);
        unsafe { program_stage1_translation(self.root_phys) }
    }
}

/// Program the calling CPU's stage-1 translation registers and enable
/// the MMU + caches with the full known [`SCTLR_MMU_ON`] value — the one
/// enable sequence [`AddressSpace::switch`] (boot CPU) and
/// [`adopt_boot_translation`] (secondary CPUs) share.
///
/// # Safety
///
/// The tables rooted at `root_phys` must identity-map the caller's `pc`,
/// stack, and every MMIO region touched before the next switch, and must
/// be observable at the point of coherency (written MMU-off, or cleaned
/// with [`clean_invalidate_range_to_poc`]). Runs with interrupts masked
/// on the calling CPU.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
unsafe fn program_stage1_translation(root_phys: u64) {
    // SAFETY: the caller asserts the mapping/coherency contract above.
    // Programming MAIR/TCR/TTBR0 then writing `SCTLR_EL1` is the
    // documented stage-1 enable sequence; the first barrier is `dsb sy`
    // — not `ish` — because tables written with the MMU off are ordered
    // in the *full-system* domain (an inner-shareable barrier is not
    // architecturally guaranteed to order them ahead of the walker's
    // first cacheable read; QEMU cannot show the difference). The
    // `tlbi vmalle1` + `dsb`/`isb` flush stale translations, `ic iallu`
    // starts the instruction cache invalid before [`SCTLR_MMU_ON`]
    // enables it, and the *whole-register* write installs a fully known
    // value — an OR of `M` into the live register would carry the
    // architecturally UNKNOWN EL1 reset bits (`WXN`, `EE`, …) into
    // translated execution, which hangs real silicon (see
    // [`SCTLR_MMU_OFF`]).
    unsafe {
        core::arch::asm!(
            "msr MAIR_EL1, {mair}",
            "msr TCR_EL1, {tcr}",
            "msr TTBR0_EL1, {ttbr}",
            "dsb sy",
            "tlbi vmalle1",
            "ic iallu",
            "dsb ish",
            "isb",
            "msr SCTLR_EL1, {sctlr}",
            "isb",
            mair = in(reg) MAIR_VALUE,
            tcr = in(reg) TCR_VALUE,
            ttbr = in(reg) root_phys,
            sctlr = in(reg) SCTLR_MMU_ON,
            options(nostack, preserves_flags),
        );
    }
}

// The `_invalidate_local_dcache_to_poc` leaf routine: invalidate the
// calling CPU's entire local data/unified cache, all levels to the Level
// of Coherence, by set/way (`dc isw`), then `dsb sy; isb`. Called by the
// `invalidate_local_dcache_to_poc` wrapper below (which carries the
// rationale and the safety contract). Implemented in assembly so the
// set/way loop's register discipline is explicit: it uses only
// caller-saved scratch (`x0`–`x11`), touches no memory, and never uses
// the stack, so it is a well-formed leaf routine.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
core::arch::global_asm!(
    ".section .text",
    ".globl _invalidate_local_dcache_to_poc",
    "_invalidate_local_dcache_to_poc:",
    "  mrs   x0, clidr_el1",
    "  and   x3, x0, #0x7000000", // LoC in CLIDR[26:24]
    "  lsr   x3, x3, #23",        // x3 = LoC << 1 (loop bound in 2*level units)
    "  cbz   x3, 5f",             // no cache levels to the PoC → done
    "  mov   x10, #0",            // x10 = 2 * current cache level
    "1:",
    "  add   x2, x10, x10, lsr #1", // x2 = 3*level = bit offset of this level's Ctype
    "  lsr   x1, x0, x2",
    "  and   x1, x1, #7", // x1 = cache type at this level
    "  cmp   x1, #2",
    "  b.lt  4f",              // <2: no data/unified cache here → skip
    "  msr   csselr_el1, x10", // select data/unified cache at this level (InD=0)
    "  isb",
    "  mrs   x1, ccsidr_el1",
    "  and   x2, x1, #7",
    "  add   x2, x2, #4", // x2 = log2(line bytes)
    "  mov   x4, #0x3ff",
    "  and   x4, x4, x1, lsr #3", // x4 = associativity - 1 (max way)
    "  clz   w5, w4",             // w5 = bit position for the way field
    "  mov   x7, #0x7fff",
    "  and   x7, x7, x1, lsr #13", // x7 = number of sets - 1 (max set)
    "2:",
    "  mov   x9, x4", // x9 = way iterator
    "3:",
    "  lsl   x6, x9, x5",
    "  orr   x11, x10, x6", // set/way operand: level | way
    "  lsl   x6, x7, x2",
    "  orr   x11, x11, x6", // | set
    "  dc    isw, x11",     // invalidate this set/way (never clean)
    "  subs  x9, x9, #1",
    "  b.ge  3b",
    "  subs  x7, x7, #1",
    "  b.ge  2b",
    "4:",
    "  add   x10, x10, #2", // next cache level (2*level units)
    "  cmp   x3, x10",
    "  b.gt  1b",
    "5:",
    "  dsb   sy",
    "  isb",
    "  ret",
);

/// Invalidate this CPU's local data cache to the Level of Coherence
/// (see the `_invalidate_local_dcache_to_poc` routine above for why
/// invalidate, not clean).
///
/// # Safety
///
/// Must run with the MMU and the data cache **off** (SCTLR.M=0, C=0), as
/// on a freshly-released secondary before [`adopt_boot_translation`]: it
/// discards the cache contents without writeback, so it is sound only
/// when no cache line holds live dirty data — which holds at that point
/// (the core has made no cacheable access yet).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
unsafe fn invalidate_local_dcache_to_poc() {
    extern "C" {
        fn _invalidate_local_dcache_to_poc();
    }
    // SAFETY: a leaf assembly routine that only issues set/way cache
    // invalidations and barriers; it clobbers caller-saved scratch, uses
    // no stack, and returns. The MMU/cache-off precondition is the
    // caller's contract.
    unsafe {
        _invalidate_local_dcache_to_poc();
    }
}

/// Enable the MMU on a freshly-started secondary core by adopting the
/// boot address space whose root [`AddressSpace::switch`] published
/// (`PARK_ROOT`) — a secondary allocates no tables of its own; it joins
/// the identity map the boot CPU already runs on.
///
/// Returns `false`, changing nothing, when no boot root has been
/// published yet: a secondary started before the boot CPU enabled its
/// MMU has no coherent tables to adopt, so it must park rather than run
/// MMU-off into the allocator's cacheable-memory requirements (fail
/// closed).
///
/// # Safety
///
/// Must be called on a secondary core with the MMU off and interrupts
/// masked, before its first atomic read-modify-write access (LDXR/STXR
/// exclusives are unreliable on MMU-off Device-typed DRAM — the boot
/// path's documented constraint). The published boot tables identity-map
/// the kernel image, the secondary stacks, and the board MMIO window for
/// the image's lifetime, which upholds [`program_stage1_translation`]'s
/// mapping contract.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub unsafe fn adopt_boot_translation() -> bool {
    let root = PARK_ROOT.load(Ordering::Acquire);
    if root == 0 {
        return false;
    }
    // A secondary core's caches are in the architecturally-UNKNOWN
    // power-on state: it may hold stale lines (from firmware or a
    // previous life) over the physical addresses RustOS now uses for the
    // boot page tables and this core's stack. Enabling the data cache
    // with those lines present lets them shadow DRAM, so the first
    // cacheable access after the MMU comes on — the table walk or a stack
    // access — intermittently reads garbage and the core faults with no
    // vectors installed (the real Pi 4 symptom: the last-released core
    // deterministically-then-intermittently never checks in). Invalidate
    // this core's local cache to the point of coherency first — discard,
    // never clean, or the garbage would be written back over the live
    // tables. The boot CPU needs no equivalent: firmware hands it clean
    // caches before the kernel runs.
    // SAFETY: runs on a secondary with the MMU and cache off (the
    // trampoline established `SCTLR_MMU_OFF`) and before any cacheable
    // access, so no live dirty line is discarded.
    unsafe { invalidate_local_dcache_to_poc() };
    // SAFETY: the caller upholds the MMU-off/interrupts-masked contract;
    // the published root is the boot space's, whose tables live (and
    // stay coherent — they were cleaned to PoC before the boot switch)
    // for the image's lifetime.
    unsafe { program_stage1_translation(root) };
    true
}

impl MmuAddressSpace for AddressSpace {
    fn map_page(&mut self, vaddr: u64, paddr: u64, flags: PageFlags) -> Result<(), MapError> {
        if (vaddr & (PAGE_SIZE as u64 - 1)) != 0 || (paddr & (PAGE_SIZE as u64 - 1)) != 0 {
            return Err(MapError::Misaligned);
        }
        if flags.is_write_exec() {
            return Err(MapError::InvalidFlags);
        }
        if self.leaf_present(vaddr) {
            return Err(MapError::AlreadyMapped);
        }
        let frames = self.frames;
        // Alignment and prior-mapping are already ruled out, so the only
        // remaining failure from the walk is frame-source exhaustion.
        self.map_4k_with_attrs(frames, vaddr, paddr, Self::leaf_attrs_for(flags))
            .ok_or(MapError::PoolExhausted)
    }

    fn translate(&self, vaddr: u64) -> Option<(u64, PageFlags)> {
        let e1 = self.root[table_index(vaddr, 1)];
        if (e1 & attrs::VALID) == 0 {
            return None;
        }
        if is_block(e1) {
            return Some((
                resolved_page(phys_from_descriptor(e1), vaddr, 30),
                page_flags_from_leaf(e1),
            ));
        }
        // SAFETY: a present table descriptor holds an output address
        // `ensure_child` wrote from `phys_of(&[u64; 512])`; identity
        // mapping makes that physical address directly dereferenceable
        // (the same round-trip `leaf_present` relies on).
        let l2 = unsafe { &*(phys_from_descriptor(e1) as *const [u64; ENTRIES_PER_TABLE]) };
        let e2 = l2[table_index(vaddr, 2)];
        if (e2 & attrs::VALID) == 0 {
            return None;
        }
        if is_block(e2) {
            return Some((
                resolved_page(phys_from_descriptor(e2), vaddr, 21),
                page_flags_from_leaf(e2),
            ));
        }
        // SAFETY: as above — a present L2 table descriptor's output
        // address is a valid identity-mapped table.
        let l3 = unsafe { &*(phys_from_descriptor(e2) as *const [u64; ENTRIES_PER_TABLE]) };
        let e3 = l3[table_index(vaddr, 3)];
        if (e3 & attrs::VALID) == 0 {
            return None;
        }
        Some((phys_from_descriptor(e3), page_flags_from_leaf(e3)))
    }

    fn unmap(&mut self, vaddr: u64) -> Result<u64, MapError> {
        if (vaddr & (PAGE_SIZE as u64 - 1)) != 0 {
            return Err(MapError::Misaligned);
        }
        // Navigate to the 4 KiB page leaf without allocating. A missing
        // level or a block leaf encountered on the way means there is no
        // 4 KiB leaf to tear down here — fail closed (the per-page unmap
        // path never shatters a block).
        let e1 = self.root[table_index(vaddr, 1)];
        if (e1 & attrs::VALID) == 0 || is_block(e1) {
            return Err(MapError::NotMapped);
        }
        // SAFETY: a present table descriptor's output address is an
        // identity-mapped table (see `translate`); `&mut self` makes the
        // exclusive borrow sound.
        let l2 = unsafe { &mut *(phys_from_descriptor(e1) as *mut [u64; ENTRIES_PER_TABLE]) };
        let e2 = l2[table_index(vaddr, 2)];
        if (e2 & attrs::VALID) == 0 || is_block(e2) {
            return Err(MapError::NotMapped);
        }
        // SAFETY: as above — a present L2 table descriptor's output
        // address is a valid identity-mapped table.
        let l3 = unsafe { &mut *(phys_from_descriptor(e2) as *mut [u64; ENTRIES_PER_TABLE]) };
        let i3 = table_index(vaddr, 3);
        let e3 = l3[i3];
        if (e3 & attrs::VALID) == 0 {
            return Err(MapError::NotMapped);
        }
        let paddr = phys_from_descriptor(e3);
        l3[i3] = 0;
        Ok(paddr)
    }

    fn root_phys(&self) -> u64 {
        self.root_phys
    }

    fn block_split_support(&self) -> BlockSplit {
        // aarch64 re-expresses a 1 GiB / 2 MiB block as a table of finer
        // leaves (`plans/PI.md` G1/G2 — the guard-page fault-form
        // foundation, host- and `-M virt`-proven).
        BlockSplit::Supported
    }

    fn split_block(&mut self, vaddr: u64) -> Result<(), MapError> {
        // The HAL view of the inherent, fully-tested `AddressSpace::split_block`
        // (G1): one body, reached either directly by the arch boot path /
        // verticals or through the HAL trait here.
        // Method-call syntax resolves to the inherent method (inherent methods
        // take precedence over a same-named trait method), so this forwards to
        // the inherent body rather than recursing into itself.
        self.split_block(vaddr)
    }

    fn prepare_guard_arena(&mut self, base: u64, len: u64) -> Result<(), MapError> {
        // The HAL view of the inherent, fully-tested
        // `AddressSpace::prepare_guard_arena` (G2): one body, reached either
        // directly by the arch boot path / verticals or through the HAL trait
        // here. As with `split_block`, inherent-method
        // resolution forwards to the inherent body rather than recursing.
        self.prepare_guard_arena(base, len)
    }

    unsafe fn activate(&self) {
        #[cfg(all(target_arch = "aarch64", target_os = "none"))]
        {
            // SAFETY: forwards to the gated stage-1 enable primitive; the
            // caller upholds the `MmuAddressSpace::activate` contract (this
            // space maps the current `pc`/`sp`/MMIO), which is exactly
            // `AddressSpace::switch`'s contract.
            unsafe { self.switch() };
        }
        #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
        {
            unreachable!("stage-1 activation is only meaningful on the aarch64 bare-metal target")
        }
    }

    unsafe fn reclaim_table_frames(&mut self) {
        // Defence in depth: the dispatcher parks a CPU off a user root at
        // every task suspend, so a dead space's root is never the active
        // translation here — but freeing the walked-from root of a live
        // regime would be catastrophic, so verify and re-park first. With
        // no park root published the frames are retired unreclaimed
        // rather than dismantling the active translation (fail closed).
        if active_root_phys() == self.root_phys && !park_kernel_root() {
            return;
        }
        let frames = self.frames;
        // A stage-1 hierarchy rooted at L1: an L1/L2 entry that is valid
        // and not a block is a table pointer; L3 (depth 2) entries are
        // page leaves and are never descended into.
        let child_of = |entry: u64, depth: usize| -> Option<u64> {
            (depth < 2 && (entry & attrs::VALID) != 0 && !is_block(entry))
                .then(|| phys_from_descriptor(entry))
        };
        // Tables are recovered from their physical address through the
        // kernel identity map — the same round-trip `translate`,
        // `leaf_present`, and `ensure_child` rely on.
        let entries_of = |phys: u64| phys as *const [u64; ENTRIES_PER_TABLE];
        // SAFETY: every phys `child_of` yields was written by
        // `ensure_child` from a `TableFrame` of `self.frames`, so it names
        // a live, identity-reachable table this hierarchy owns; the guard
        // above upholds the not-active contract the caller asserts, and
        // `self` is borrowed mutably so no other reference walks the
        // tables.
        unsafe {
            reclaim_hierarchy(self.root_phys, &child_of, &entries_of, &mut |phys| {
                frames.free_table(phys);
            });
        }
    }
}

impl TlbShootdown for AddressSpace {
    fn flush_page(&mut self, vaddr: u64) {
        invalidate_page_inner_shareable(vaddr);
    }

    fn flush_range(&mut self, _start_vaddr: u64, page_count: usize) {
        if page_count != 0 {
            invalidate_all_inner_shareable();
        }
    }
}

/// Invalidate, on every PE in the inner-shareable domain, the stage-1
/// EL1&0 TLB entries for the 4 KiB page containing `vaddr` (all ASIDs).
///
/// This is the single instruction sequence shared by both the *local*
/// per-page flush ([`TlbShootdown::flush_page`]) and the *cross-CPU*
/// shootdown ([`rustos_arch_api::CrossCpuTlbShootdown::shootdown_page`] on
/// [`crate::kernel_arch::Aarch64Arch`]): `tlbi vaae1is` is the
/// inner-shareable *broadcast* variant, so the "local" and "cross-CPU"
/// shootdowns are literally the same operation on aarch64 — there is one
/// implementation, not two.
pub(crate) fn invalidate_page_inner_shareable(vaddr: u64) {
    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
    {
        // SAFETY: `tlbi vaae1is` invalidates the inner-shareable TLB
        // entries for the page named by its operand (VA[55:12], all
        // ASIDs); the `dsb`/`isb` barriers order the invalidation and
        // make it visible before the next translation. It touches no
        // memory and only discards a cached translation. No Rust
        // spelling exists.
        let va_page = vaddr >> 12;
        unsafe {
            core::arch::asm!(
                "dsb ishst",
                "tlbi vaae1is, {page}",
                "dsb ish",
                "isb",
                page = in(reg) va_page,
                options(nostack, preserves_flags),
            );
        }
    }
    #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
    {
        // The host has no TLB to invalidate; a flush is vacuous.
        let _ = vaddr;
    }
}

/// Invalidate every stage-1 EL1&0 translation on every PE in the
/// inner-shareable domain.
///
/// A large newly-installed range uses this broad operation once instead of
/// issuing a barrier-bracketed `tlbi` for every 4 KiB leaf. Over-invalidation
/// is safe, while the single broadcast keeps the range visible to every PE
/// before execution continues.
fn invalidate_all_inner_shareable() {
    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
    {
        // SAFETY: `tlbi vmalle1is` invalidates all stage-1 EL1&0
        // translations in the inner-shareable domain. The barriers order
        // prior page-table writes before invalidation and subsequent
        // translations after completion. It only discards cached state.
        unsafe {
            core::arch::asm!(
                "dsb ishst",
                "tlbi vmalle1is",
                "dsb ish",
                "isb",
                options(nostack, preserves_flags),
            );
        }
    }
}

/// The permanent kernel translation root a CPU parks on whenever it must
/// leave a user root — published set-once by the first
/// `AddressSpace::switch` (the boot space, whose tables live for the
/// image's lifetime), read by [`park_kernel_root`]. `0` means "not yet
/// published" (the boot space's root table is never at physical 0).
static PARK_ROOT: AtomicU64 = AtomicU64::new(0);

/// Park the calling CPU's low translation regime on the published boot
/// kernel root, so no user space's root remains active after its task
/// suspends or exits. Returns `false`, changing nothing, when no park
/// root has been published yet (fail closed).
///
/// The dispatcher calls this after every switch-back from a user task;
/// address-space teardown calls it defensively before dismantling a root
/// that is somehow still active.
pub fn park_kernel_root() -> bool {
    let root = PARK_ROOT.load(Ordering::Acquire);
    if root == 0 {
        return false;
    }
    // SAFETY: the published root is the boot space's, which identity-maps
    // the kernel window and the board MMIO for the image's lifetime —
    // exactly `activate_user_root`'s contract (inert on the host, where
    // the root is never published anyway).
    unsafe { activate_user_root(root) };
    true
}

/// The physical root of the calling CPU's active low translation regime
/// (`TTBR0_EL1`'s base address), or `0` on the host, which has no
/// translation registers.
fn active_root_phys() -> u64 {
    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
    {
        let ttbr: u64;
        // SAFETY: reading `TTBR0_EL1` observes the active root without
        // side effects; no Rust spelling exists for the system register.
        unsafe {
            core::arch::asm!("mrs {v}, TTBR0_EL1", v = out(reg) ttbr, options(nostack, preserves_flags, nomem));
        }
        // Mask the ASID ([63:48]) and CnP ([0]) fields to the table base.
        ttbr & 0x0000_FFFF_FFFF_FFFE
    }
    #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
    {
        0
    }
}

/// Reactivate `root_phys` as the active stage-1 EL1&0 translation root
/// (reprogram `TTBR0_EL1`) on a CPU whose MMU is already enabled.
///
/// This is the SP2b user-kthread `pre_resume` primitive (`plans/SPAWN.md`
/// SP2): immediately before the kernel `eret`s back into a user task's
/// EL0, that task's own page-table root must be installed so its
/// translations — and only its — are in force, keeping sibling processes
/// hardware-isolated. It takes only the `u64` root, so
/// the per-task hook that calls it captures a plain word and stays `Send`.
///
/// Unlike [`AddressSpace::switch`] this does **not** touch `MAIR_EL1` /
/// `TCR_EL1` / `SCTLR_EL1.M`: the MMU is already on with the boot
/// translation controls in force, and only the low (`TTBR0_EL1`)
/// translation regime changes between user spaces.
///
/// # Safety
///
/// The MMU must already be enabled, and the L1 table at `root_phys` must
/// map the currently-executing kernel `pc`, `sp`, and the MMIO the code
/// touches identically to the outgoing root — every RustOS user space
/// identity-maps the low kernel window, so this holds for any task root,
/// but a `root_phys` that does not faults the CPU on its next access.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub unsafe fn activate_user_root(root_phys: u64) {
    // SAFETY: writing `TTBR0_EL1` swaps the low translation regime; the
    // `dsb`/`tlbi vmalle1`/`dsb`/`isb` sequence flushes the stale EL1&0
    // entries and makes the new root in force before the next access. No
    // memory is touched and no Rust spelling exists for these system
    // registers. The caller's contract guarantees the new root covers the
    // running kernel context.
    unsafe {
        core::arch::asm!(
            "msr TTBR0_EL1, {ttbr}",
            "dsb ish",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            ttbr = in(reg) root_phys,
            options(nostack, preserves_flags),
        );
    }
}

/// Host substitute: reprogramming `TTBR0_EL1` is meaningful only on the
/// bare-metal aarch64 target. Never linked into a kernel image and never
/// reached on the host (the QEMU verticals exercise the real switch).
///
/// # Safety
///
/// Carries the same contract as the bare-metal definition above (MMU
/// enabled; `root_phys` maps the running kernel context), so the two
/// `cfg` arms present one `unsafe` API. The host body is inert.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
pub unsafe fn activate_user_root(root_phys: u64) {
    let _ = root_phys;
}

/// Populate the freshly-allocated table `child` with 512 descriptors that
/// reproduce the leaf `block` at the next finer granularity, preserving
/// every attribute bit.
///
/// `sub_shift` is the base-2 log of each sub-entry's coverage (21 for the
/// 2 MiB sub-blocks an L1 block shatters into, 12 for the 4 KiB pages an
/// L2 block shatters into); `page` selects an L3 *page* descriptor
/// (`TABLE_OR_PAGE` set) when shattering to 4 KiB, versus a finer *block*
/// (bit clear) at 2 MiB. Only the output address changes per sub-entry —
/// `block & !ADDR_MASK` captures `VALID` plus every lower (`[11:2]`) and
/// upper (`[63:48]`, incl. `PXN`/`UXN`) attribute bit, so the finer
/// descriptors map the same memory with identical permissions
/// (one attribute vocabulary, never re-derived).
fn shatter_block_into(
    child: &mut [u64; ENTRIES_PER_TABLE],
    block: u64,
    sub_shift: u32,
    page: bool,
) {
    let base = phys_from_descriptor(block);
    let attr_bits = block & !ADDR_MASK;
    let sub_size = 1u64 << sub_shift;
    for (i, slot) in child.iter_mut().enumerate() {
        let sub_pa = base + (i as u64) * sub_size;
        let mut desc = (sub_pa & ADDR_MASK) | attr_bits;
        if page {
            desc |= attrs::TABLE_OR_PAGE;
        } else {
            desc &= !attrs::TABLE_OR_PAGE;
        }
        *slot = desc;
    }
}

// `&mut [u64; 512]` in, `&'static mut [u64; 512]` out: the returned
// reference points at a freshly-alloc'd table from `frames` or at a
// sibling recovered through the identity map, never a borrow of
// `parent` — exactly the shape `mut_from_ref` flags.
#[allow(clippy::mut_from_ref)]
fn ensure_child(
    parent: &mut [u64; ENTRIES_PER_TABLE],
    idx: usize,
    frames: &'static dyn PageTableFrames,
) -> Option<&'static mut [u64; ENTRIES_PER_TABLE]> {
    let entry = parent[idx];
    if (entry & attrs::VALID) != 0 {
        if is_block(entry) {
            // A block where we expected a table pointer: refuse rather
            // than shatter a large mapping silently.
            return None;
        }
        let phys = phys_from_descriptor(entry);
        // SAFETY: every non-block valid entry was inserted below with an
        // output address derived from a `TableFrame`, so the round-trip
        // is valid; identity mapping means the physical address is also
        // the address we dereference.
        let child: &'static mut [u64; ENTRIES_PER_TABLE] =
            unsafe { &mut *(phys as *mut [u64; ENTRIES_PER_TABLE]) };
        Some(child)
    } else {
        let TableFrame { phys, entries } = frames.alloc_table()?;
        parent[idx] = table_descriptor(phys);
        Some(entries)
    }
}

fn phys_of(table: &[u64; ENTRIES_PER_TABLE]) -> u64 {
    // Identity-mapped: virtual == physical for everything the kernel
    // owns, because the boot trampoline runs with the MMU off and the
    // gigapage identity map preserves it.
    table.as_ptr() as u64
}

/// Decode a stage-1 leaf (block or page) descriptor's attributes back
/// into the neutral [`PageFlags`] (the inverse of
/// [`AddressSpace::leaf_attrs_for`]). A valid leaf is always readable;
/// the AP field decides writability and EL0 reachability, the
/// execute-never bit for the leaf's privilege level decides
/// executability, and the `MAIR` attribute index decides Device.
fn page_flags_from_leaf(desc: u64) -> PageFlags {
    let mut out = PageFlags::READ;
    let ap = desc & (0b11 << 6);
    let user = ap == attrs::AP_RW_EL0 || ap == attrs::AP_RO_EL0;
    if ap == attrs::AP_RW_EL1 || ap == attrs::AP_RW_EL0 {
        out = out | PageFlags::WRITE;
    }
    if user {
        out = out | PageFlags::USER;
        if desc & attrs::UXN == 0 {
            out = out | PageFlags::EXEC;
        }
    } else if desc & attrs::PXN == 0 {
        out = out | PageFlags::EXEC;
    }
    // The `MAIR` attribute index (bits [4:2]) selects the memory type:
    // index 1 = Device-nGnRE, index 2 = Normal Non-Cacheable (a coherent
    // DMA buffer), index 0 = cacheable Normal (no attribute bit).
    let attr_idx = desc & (0b111 << 2);
    if attr_idx == attrs::ATTR_IDX_DEVICE {
        out = out | PageFlags::DEVICE;
    } else if attr_idx == attrs::ATTR_IDX_NORMAL_NC {
        if desc & attrs::SW_WRITE_COMBINE != 0 {
            out = out | PageFlags::WRITE_COMBINE;
        } else {
            out = out | PageFlags::DMA_COHERENT;
        }
    }
    out
}

/// 4 KiB-aligned physical address `vaddr` resolves to under a leaf whose
/// region starts at `leaf_base` and spans `1 << region_shift` bytes
/// (30 = L1 block, 21 = L2 block, 12 = L3 page). The page offset is
/// dropped so the result is page-aligned (the HAL `translate` contract
/// reports the 4 KiB page base).
fn resolved_page(leaf_base: u64, vaddr: u64, region_shift: u32) -> u64 {
    let region_mask = (1u64 << region_shift) - 1;
    (leaf_base + (vaddr & region_mask)) & !((PAGE_SIZE as u64) - 1)
}

#[cfg(test)]
#[path = "paging_tests.rs"]
mod tests;
