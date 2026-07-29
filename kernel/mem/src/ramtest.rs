//! Early-boot physical-RAM self-test.
//!
//! Run once, on the boot CPU, before the [`crate::FrameAllocator`] hands
//! out a single frame: every byte of *usable* RAM in the boot memory map is
//! written and read back through the kernel's direct physical map to prove
//! it stores what was written. A machine that silently mis-reports its RAM,
//! or whose DRAM has a stuck data bit or a broken address line, is caught
//! here — before that memory is trusted to hold a page table, a
//! capability token, or a user's data — and the boot is halted rather than
//! run on faulty storage (fail closed).
//!
//! # Why this is safe to run
//!
//! The test is destructive: it overwrites every word it touches. It is
//! confined to [`RegionKind::Usable`] regions, which are by definition the
//! RAM the allocator has not yet handed out and nothing is using — the
//! kernel image, its stack, the boot page tables, the device tree and every
//! other live datum sit in [`RegionKind::Reserved`] regions the boot map
//! already carves out, and are never touched. The sampled words the test
//! writes are restored to zero, but the test does **not** scrub whole
//! regions: the allocator's consumers zero their own frames before use (the
//! page-table frame source hands back zero-initialised frames, anonymous and
//! DMA memory is zeroed on map, the user stack is zeroed on spawn), so a
//! clean frame is each consumer's own guarantee, not this test's.
//!
//! # Defeating the cache
//!
//! Reads and writes go through the *cacheable* direct map, so a naive
//! write-then-read would be served from the CPU cache and never reach the
//! DRAM cell under test. Before reading a cell back the engine flushes just
//! that cell's cache line ([`WordWindow::flush_word`], backed by
//! [`PhysMap::clean_invalidate`]), which writes the dirty line back to DRAM
//! and drops the cached copy, so the read observes the value the DRAM
//! actually holds. On an I/O-coherent host or simulator the flush is a
//! documented no-op and the model is identical.
//!
//! # The algorithm
//!
//! This is a *boot sanity check*, not an exhaustive march test: it must
//! finish in a couple of seconds on many gigabytes, so it does not touch
//! every byte. Two complementary passes are applied to each window (after
//! Michael Barr, *"Software-Based Memory Testing"*, 2000):
//!
//! * a whole-window **address-line** test that walks power-of-two offsets to
//!   catch an address bit that is stuck or shorted (writes that land in the
//!   wrong cell) in `O(log² n)` accesses, covering the address decode across
//!   the full span, and
//! * a sampling **device** test that proves one word per 4 KiB holds both a
//!   `1` and a `0` (writing an alternating pattern then its complement and
//!   reading each back), catching a stuck data bit, row, column, or bank —
//!   the faults that occur in practice, which span many contiguous cells.
//!
//! Only the address pass's handful of offsets and the device pass's periodic
//! sample are ever written, read, or flushed, so the whole test costs
//! `O(usable_bytes / sample_interval)` cache-line accesses rather than
//! `O(usable_bytes)` — the difference between seconds and minutes on a large
//! machine. The words between samples are left untouched; a lone single-cell
//! fault that falls between two samples is the coverage this check trades
//! away for speed (that is `memtest86`'s job). Every cell either pass writes
//! is restored to zero, so the test leaves no pattern behind.
//!
//! The engine itself is architecture-neutral and host-testable: it operates
//! over the [`WordWindow`] trait, which the production path backs with the
//! arch direct map and the unit tests back with an ordinary host buffer
//! (including deliberately faulty ones).

use core::ptr::NonNull;

use crate::bootinfo::{BootMemoryMap, MemoryRegion, RegionKind};
use crate::frame::{FrameAllocator, PhysAddr, PAGE_SIZE};
use crate::phys::PhysMap;
use crate::ptr::offset_within;

/// The machine word the test reads and writes, one aligned unit at a time.
///
/// Fixed at 64 bits (not `usize`) so the patterns and the fault report mean
/// the same thing on every target regardless of pointer width; the direct
/// map hands out frame-aligned pointers, so every access is naturally
/// aligned.
pub type Word = u64;

/// Bytes in one [`Word`].
const WORD_BYTES: usize = core::mem::size_of::<Word>();

/// Bytes tested per progress step.
///
/// Each window is tested in full (address then device passes) before the
/// next, and the caller is told the cumulative byte total after every one,
/// so the on-screen counter advances smoothly rather than jumping a whole
/// region at a time. 2 MiB is a fine granularity for the counter while still
/// giving the address pass a wide span of power-of-two offsets to walk.
pub const PROGRESS_STEP_BYTES: usize = 2 * 1024 * 1024;

/// The location and nature of the first detected RAM fault.
///
/// `phys` is the physical address of the faulting word; `expected` is the
/// value written and `observed` the value read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RamFault {
    /// Physical address of the word that failed to read back correctly.
    pub phys: PhysAddr,
    /// The value the engine wrote.
    pub expected: Word,
    /// The value the engine read back.
    pub observed: Word,
}

/// A contiguous run of testable RAM, accessed one aligned [`Word`] at a time.
///
/// Reads and writes hit the underlying storage directly (a direct-mapped
/// physical frame in production, a host buffer under test);
/// [`flush_word`](WordWindow::flush_word) forces a prior
/// [`write`](WordWindow::write) out to — and drops any cached copy of — that
/// storage so a following [`read`](WordWindow::read) observes what the
/// storage actually holds.
pub trait WordWindow {
    /// Number of [`Word`]s in the window.
    fn words(&self) -> usize;
    /// Read the word at index `i` (`i < words()`).
    fn read(&self, i: usize) -> Word;
    /// Write `value` to the word at index `i` (`i < words()`).
    fn write(&self, i: usize, value: Word);
    /// Flush the single word at index `i` (its cache line) back to the
    /// backing store and drop the cached copy, so the next
    /// [`read`](Self::read) of `i` observes the value the DRAM cell actually
    /// holds rather than a cached copy.
    ///
    /// The test only ever touches scattered individual words (the address
    /// pass's power-of-two offsets, the device pass's strided sample), so a
    /// per-word flush is exactly what is needed: a whole-window flush would
    /// pay to write back and invalidate every cache line of RAM that is
    /// never read, which is the `O(all bytes)` cost this sampling test
    /// exists to avoid.
    fn flush_word(&self, i: usize);
}

/// Test pattern: alternating `1010…` bits.
const PATTERN: Word = 0xAAAA_AAAA_AAAA_AAAA;
/// The complement of [`PATTERN`]: alternating `0101…` bits.
const ANTIPATTERN: Word = 0x5555_5555_5555_5555;

/// A marker value unique to word offset `off`, for the address pass.
///
/// Each power-of-two offset (and offset 0) is stamped with its own value so
/// that an address line which makes two offsets alias shows up as a marker
/// read back from the wrong place. Folding the offset into [`PATTERN`] with
/// exclusive-or keeps every marker distinct *and* exercises a spread of
/// set/clear data bits, so the markers do not read back correctly by accident
/// on a stuck-bit cell.
fn address_marker(off: usize) -> Word {
    (off as Word) ^ PATTERN
}

/// Walk the address lines of `w`, catching a bit that is stuck or shorted.
///
/// Stamps offset 0 and every power-of-two word offset with its own
/// [`address_marker`], flushing each to DRAM, then reads them all back: if a
/// stuck or shorted address line makes a write to one offset land on
/// another, the aliased-over offset reads back the wrong marker and the pass
/// fails closed at that offset. Exercising every power-of-two offset touches
/// each address bit individually — the textbook quick address-line test — in
/// `O(log n)` accesses. The window base is index 0, so its physical address
/// is `base`.
fn address_pass<W: WordWindow>(w: &W, base: PhysAddr) -> Result<(), RamFault> {
    let n = w.words();
    if n < 2 {
        // A single word has no address bit to walk; the device pass still
        // proves that lone cell.
        return Ok(());
    }
    // Stamp offset 0 and each power-of-two offset with its marker, flushing
    // each so the later read-back reaches DRAM rather than a cached copy.
    w.write(0, address_marker(0));
    w.flush_word(0);
    let mut off = 1;
    while off < n {
        w.write(off, address_marker(off));
        w.flush_word(off);
        off <<= 1;
    }
    // Read every marker back; an aliasing address line shows up as a marker
    // that landed on, or was overwritten at, the wrong offset.
    check(w, base, 0, address_marker(0))?;
    let mut off = 1;
    while off < n {
        check(w, base, off, address_marker(off))?;
        off <<= 1;
    }
    // Restore the handful of cells this pass wrote back to zero (no flush
    // needed — nothing reads them again, and the allocator's consumers zero
    // their own frames), so the pass leaves no marker behind in RAM.
    w.write(0, 0);
    let mut off = 1;
    while off < n {
        w.write(off, 0);
        off <<= 1;
    }
    Ok(())
}

/// Bytes between the device pass's sampled words.
///
/// One aligned word is tested per this span. It is exactly the page size, so
/// the sample lands one word in every page. Chosen so the whole test still
/// finishes in a few seconds on many gigabytes (measured under QEMU/TCG,
/// where a cache-line flush is far dearer than on real silicon) while
/// hitting every stuck DRAM cell, row, column, or bank many times over —
/// those faults span far more than this interval.
const DEVICE_SAMPLE_INTERVAL_BYTES: usize = 4 * 1024;

/// Word stride of the device pass's sample: one word tested per this many.
///
/// The device pass deliberately does **not** touch every cell — that is the
/// `O(all bytes)` write-back-and-verify cost this boot-time test exists to
/// avoid. Instead it samples one aligned word per
/// [`DEVICE_SAMPLE_INTERVAL_BYTES`], while the whole-window [`address_pass`]
/// proves the address decode across the full span. A stuck DRAM cell, row,
/// column, or bank — the faults that actually occur — spans many contiguous
/// cells and is caught by the sample; a lone single-cell fault that falls
/// between two samples is the coverage a *boot sanity check* trades away to
/// finish in a couple of seconds on many gigabytes. An exhaustive march test
/// is `memtest86`'s job, not the boot path's.
const DEVICE_SAMPLE_STRIDE_WORDS: usize = DEVICE_SAMPLE_INTERVAL_BYTES / WORD_BYTES;

/// Prove a sampled word of every page holds both a `1` and a `0` — the fast,
/// sampling device test.
///
/// For each sampled word (one per [`DEVICE_SAMPLE_STRIDE_WORDS`]) the pass
/// drives the cell to [`PATTERN`] and reads it back, then to [`ANTIPATTERN`]
/// and reads it back: between the two, every one of that word's 64 bits is
/// proven to hold both a `1` and a `0`, so a bit stuck at either polarity
/// mismatches on one of the two reads — full stuck-at coverage *of the
/// sampled cell*. The cell is then set back to zero (a plain store, no
/// flush: nothing reads it again here, and the allocator's consumers zero
/// their own frames), so the pass leaves no pattern behind.
///
/// Only the sampled words are ever written, read, or flushed, so the pass
/// costs `O(words / stride)` accesses rather than `O(words)` — the
/// difference between a couple of seconds and many minutes on a large
/// machine. The unsampled words are left untouched; the allocator's
/// consumers zero their own frames before use, so this pass owes them no
/// whole-region scrub.
fn device_pass<W: WordWindow>(w: &W, base: PhysAddr) -> Result<(), RamFault> {
    let n = w.words();
    let mut i = 0;
    while i < n {
        prove_cell(w, base, i)?;
        i += DEVICE_SAMPLE_STRIDE_WORDS;
    }
    Ok(())
}

/// Drive word `i` to each bit polarity in turn, reading it back through a
/// flush so the read observes DRAM and not the cache, then set it back to
/// zero. Fails closed naming the cell on the first mismatch.
fn prove_cell<W: WordWindow>(w: &W, base: PhysAddr, i: usize) -> Result<(), RamFault> {
    for pattern in [PATTERN, ANTIPATTERN] {
        w.write(i, pattern);
        w.flush_word(i);
        check(w, base, i, pattern)?;
    }
    // Leave the sampled cell zeroed; no flush — nothing reads it again in
    // this test and each consumer zeroes its own frames before use.
    w.write(i, 0);
    Ok(())
}

/// Read word `i` and fail closed with its physical address if it does not
/// equal `expected`.
fn check<W: WordWindow>(w: &W, base: PhysAddr, i: usize, expected: Word) -> Result<(), RamFault> {
    let observed = w.read(i);
    if observed == expected {
        Ok(())
    } else {
        let phys = PhysAddr::new(base.as_u64() + (i as u64) * WORD_BYTES as u64);
        Err(RamFault {
            phys,
            expected,
            observed,
        })
    }
}

/// Run every pass over one window: the whole-window address-line pass then
/// the sampling device pass. Returns the first mismatch, or `Ok` if every
/// tested cell read back correctly.
fn test_window<W: WordWindow>(w: &W, base: PhysAddr) -> Result<(), RamFault> {
    address_pass(w, base)?;
    device_pass(w, base)?;
    Ok(())
}

/// A [`WordWindow`] over a contiguous physical range reached through the
/// kernel's direct physical map.
struct PhysWindow<'a, M: PhysMap + ?Sized> {
    /// CPU pointer to the first byte of the window (from [`PhysMap::translate`]).
    ptr: NonNull<u8>,
    /// Window length in bytes (a multiple of [`WORD_BYTES`]).
    len: usize,
    /// Number of whole words in the window.
    words: usize,
    /// Physical base address the window covers, for [`PhysMap::clean_invalidate`].
    base: PhysAddr,
    /// The direct map, used only to flush the window.
    physmap: &'a M,
}

impl<'a, M: PhysMap + ?Sized> PhysWindow<'a, M> {
    /// Map `[base, base + len)` for testing, or [`None`] if the direct map
    /// does not cover it (the caller then leaves that range untested rather
    /// than synthesising a pointer of its own — fail closed).
    ///
    /// `len` must be a non-zero multiple of [`WORD_BYTES`]; the caller only
    /// ever passes frame-aligned spans, which satisfy both.
    fn new(physmap: &'a M, base: PhysAddr, len: usize) -> Option<Self> {
        if len == 0 || len % WORD_BYTES != 0 {
            return None;
        }
        let ptr = physmap.translate(base, len)?;
        Some(Self {
            ptr,
            len,
            words: len / WORD_BYTES,
            base,
            physmap,
        })
    }

    /// CPU pointer to word `i`, or [`None`] if `i` is out of range.
    ///
    /// Routes the offset through the crate's bounds-checked pointer helper,
    /// so this module performs no raw pointer arithmetic of its own.
    // The `*mut u8 -> *mut Word` cast is always aligned: `translate` hands
    // back a frame-aligned base and `offset` is a whole multiple of the word
    // size, so the result is 8-aligned. `cast_ptr_alignment` cannot see that
    // invariant, so it is allowed here with the reason stated rather than
    // silenced blindly.
    #[allow(clippy::cast_ptr_alignment)]
    fn word_ptr(&self, i: usize) -> Option<*mut Word> {
        let offset = i.checked_mul(WORD_BYTES)?;
        let byte = offset_within(self.ptr.as_ptr(), self.len, offset)?;
        Some(byte.cast::<Word>())
    }
}

impl<M: PhysMap + ?Sized> WordWindow for PhysWindow<'_, M> {
    fn words(&self) -> usize {
        self.words
    }

    fn read(&self, i: usize) -> Word {
        let ptr = self
            .word_ptr(i)
            .expect("index is bounds-checked by the engine");
        // SAFETY: `word_ptr` proved `[i*8, i*8+8)` lies inside the mapped
        // window, and the window base came from `translate`, which promises
        // the whole span is a valid, uniquely-owned direct-map alias of live
        // RAM. The pointer is word-aligned because the physical base is
        // frame-aligned and the offset is a multiple of the word size. A
        // volatile read is required so the compiler cannot fold the
        // write-then-read the test relies on into a constant.
        unsafe { ptr.read_volatile() }
    }

    fn write(&self, i: usize, value: Word) {
        let ptr = self
            .word_ptr(i)
            .expect("index is bounds-checked by the engine");
        // SAFETY: as in `read` — the pointer is in-bounds, word-aligned, and
        // uniquely owned. A volatile write keeps the store from being elided
        // as dead once a later pass overwrites the same cell.
        unsafe { ptr.write_volatile(value) }
    }

    fn flush_word(&self, i: usize) {
        let phys = PhysAddr::new(self.base.as_u64() + (i as u64) * WORD_BYTES as u64);
        self.physmap.clean_invalidate(phys, WORD_BYTES);
    }
}

/// Round `addr` up to the next multiple of `align` (a power of two),
/// saturating rather than wrapping at the top of the address space.
fn align_up(addr: u64, align: u64) -> u64 {
    let mask = align - 1;
    addr.saturating_add(mask) & !mask
}

/// Round `addr` down to the previous multiple of `align` (a power of two).
fn align_down(addr: u64, align: u64) -> u64 {
    addr & !(align - 1)
}

/// Test every usable region of `map`, one progress step at a time.
///
/// Each [`RegionKind::Usable`] region is rounded *inward* to whole frames
/// (matching the frame allocator, so no partial frame is ever touched) and
/// walked in [`PROGRESS_STEP_BYTES`] windows. After each window passes,
/// `on_progress` is called with the cumulative number of bytes verified so
/// far, so the caller can advance an on-screen counter.
///
/// Returns the total number of usable bytes verified on success, or the
/// first [`RamFault`] found. A region the direct map cannot reach is left
/// untested (and uncounted) rather than trusted — the boot continues on the
/// RAM that *was* proven; it never fabricates a pass for memory it could not
/// read.
pub fn run<M, F>(map: &BootMemoryMap, physmap: &M, mut on_progress: F) -> Result<u64, RamFault>
where
    M: PhysMap + ?Sized,
    F: FnMut(u64),
{
    let mut tested: u64 = 0;
    for region in map.regions() {
        if region.kind != RegionKind::Usable {
            continue;
        }
        let Some(region_end) = region.end() else {
            continue;
        };
        let start = align_up(region.start.as_u64(), PAGE_SIZE as u64);
        let end = align_down(region_end.as_u64(), PAGE_SIZE as u64);
        let mut addr = start;
        while addr < end {
            let remaining = end - addr;
            let chunk = remaining.min(PROGRESS_STEP_BYTES as u64);
            let Ok(chunk_len) = usize::try_from(chunk) else {
                break;
            };
            if let Some(window) = PhysWindow::new(physmap, PhysAddr::new(addr), chunk_len) {
                test_window(&window, PhysAddr::new(addr))?;
                tested += chunk;
                on_progress(tested);
            }
            addr += chunk;
        }
    }
    Ok(tested)
}

/// Test one caller-**owned**, frame-aligned physical window
/// `[base, base + len)` through `physmap`, running the whole-window
/// address-line pass and the sampling device pass over it.
///
/// Unlike [`run`], which is sound only on the pre-allocator boot path where
/// every `Usable` region is by definition idle, this tests a window the
/// caller *owns* — frames it has itself allocated from the
/// [`crate::FrameAllocator`] and will free afterwards. The passes write
/// markers into the window and restore them to zero, so running them over
/// memory another part of the running kernel holds would corrupt it; a caller
/// that owns the frames keeps the operation non-destructive to live state
/// while still exercising real DRAM cells (the per-word flush defeats the
/// cache exactly as on the boot path). This is the entry point the pre-boot
/// Supervisor's `memtest` drives, one owned frame span at a time, so its
/// heavier repeated-pass test never touches RAM in use.
///
/// `len` must be a non-zero multiple of the word size; a whole frame count
/// satisfies it. Returns:
///
/// * `Ok(true)` — the window was mapped and every tested cell read back
///   correctly.
/// * `Ok(false)` — the direct map does not cover the window, so it was left
///   untested rather than trusted (fail closed; the caller reports the skip).
/// * `Err(`[`RamFault`]`)` — the first cell that failed to read back.
pub fn test_owned_window<M>(physmap: &M, base: PhysAddr, len: usize) -> Result<bool, RamFault>
where
    M: PhysMap + ?Sized,
{
    match PhysWindow::new(physmap, base, len) {
        Some(window) => {
            test_window(&window, base)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// One whole-RAM test pattern the machine-takeover memtest applies to every
/// word of every usable frame.
///
/// A single *test loop* runs [`ALL`](RamTestPattern::ALL) in order over all
/// of RAM; the Supervisor's `memtest` repeats that loop until the machine is
/// reset. Each pattern targets a different fault class (after Michael Barr,
/// *"Software-Based Memory Testing"*, 2000, and the classic memtest86 suite),
/// so the whole loop is thorough rather than fast:
///
/// * [`OwnAddress`](RamTestPattern::OwnAddress) — an *address-in-address*
///   pass: every word is stamped with a value derived from its own offset and
///   read back, so a stuck or shorted address line that makes two cells alias
///   is caught by a marker read from the wrong place.
/// * [`MovingInversionsZeros`](RamTestPattern::MovingInversionsZeros) and
///   [`MovingInversionsCheckerboard`](RamTestPattern::MovingInversionsCheckerboard)
///   — *moving-inversions* passes with all-zeros/all-ones and the
///   `0xAA`/`0x55` checkerboard: the window is filled, then walked ascending
///   (verify, write complement) and descending (verify, write back), so every
///   bit of every cell holds both polarities and an inter-cell coupling fault
///   is exercised in both address directions.
/// * [`WalkingOnes`](RamTestPattern::WalkingOnes) and
///   [`WalkingZeros`](RamTestPattern::WalkingZeros) — a walking single-bit
///   pattern whose set (or clear) bit advances with the word index, catching
///   data-bus and adjacent-bit coupling faults a uniform fill cannot.
///
/// Every pattern touches **every** word of the window and does not restore
/// it: the caller owns the whole machine and it never resumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RamTestPattern {
    /// Stamp each word with its own address and read it back.
    OwnAddress,
    /// Moving inversions between all-zeros and all-ones.
    MovingInversionsZeros,
    /// Moving inversions between the `0xAA…`/`0x55…` checkerboard halves.
    MovingInversionsCheckerboard,
    /// A walking one-hot bit that advances with the word index.
    WalkingOnes,
    /// A walking single-zero bit that advances with the word index.
    WalkingZeros,
}

impl RamTestPattern {
    /// The patterns one complete test loop applies, in order.
    pub const ALL: &'static [RamTestPattern] = &[
        RamTestPattern::OwnAddress,
        RamTestPattern::MovingInversionsZeros,
        RamTestPattern::MovingInversionsCheckerboard,
        RamTestPattern::WalkingOnes,
        RamTestPattern::WalkingZeros,
    ];

    /// A short, stable display name for the progress UI.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            RamTestPattern::OwnAddress => "own-address",
            RamTestPattern::MovingInversionsZeros => "moving inversions (zeros/ones)",
            RamTestPattern::MovingInversionsCheckerboard => "moving inversions (checkerboard)",
            RamTestPattern::WalkingOnes => "walking ones",
            RamTestPattern::WalkingZeros => "walking zeros",
        }
    }
}

/// The walking-bit value for word `i`: a lone set bit (or, when `zeros`, a
/// lone clear bit) whose position advances with the word index, so a run of
/// cells exercises every data-bus line in turn.
fn walking_value(i: usize, zeros: bool) -> Word {
    let bit = 1u64 << ((i as u64) & (u64::from(Word::BITS) - 1));
    if zeros {
        !bit
    } else {
        bit
    }
}

/// Read word `i` back and, if it does not equal `expected`, report the fault
/// through `on_fault` and keep going.
///
/// Unlike [`check`], this never returns early: the takeover memtest logs each
/// bad cell and continues testing the rest of RAM (and keeps looping), so a
/// single faulty cell does not hide every other one behind it.
fn verify_word(
    w: &dyn WordWindow,
    base: PhysAddr,
    i: usize,
    expected: Word,
    on_fault: &mut dyn FnMut(RamFault),
) {
    let observed = w.read(i);
    if observed != expected {
        let phys = PhysAddr::new(base.as_u64() + (i as u64) * WORD_BYTES as u64);
        on_fault(RamFault {
            phys,
            expected,
            observed,
        });
    }
}

/// A moving-inversions pass over the whole window with `value` and its
/// complement, reporting every mismatch through `on_fault` without stopping.
fn moving_inversions(
    w: &dyn WordWindow,
    base: PhysAddr,
    value: Word,
    on_fault: &mut dyn FnMut(RamFault),
) {
    let n = w.words();
    let complement = !value;
    for i in 0..n {
        w.write(i, value);
        w.flush_word(i);
    }
    // Ascending: expect the value, drive the complement in its place.
    for i in 0..n {
        verify_word(w, base, i, value, on_fault);
        w.write(i, complement);
        w.flush_word(i);
    }
    // Descending: expect the complement, drive the value back.
    for i in (0..n).rev() {
        verify_word(w, base, i, complement, on_fault);
        w.write(i, value);
        w.flush_word(i);
    }
}

/// Apply one [`RamTestPattern`] to every word of window `w`, reporting each
/// mismatch through `on_fault` and continuing.
///
/// Every pattern writes the whole window and reads it back through a per-word
/// flush (so the read observes DRAM, not the cache) and leaves the window
/// holding its final pattern — the caller owns the machine and it never
/// resumes, so nothing is restored.
fn sweep_window(
    w: &dyn WordWindow,
    base: PhysAddr,
    pattern: RamTestPattern,
    on_fault: &mut dyn FnMut(RamFault),
) {
    let n = w.words();
    match pattern {
        RamTestPattern::OwnAddress => {
            for i in 0..n {
                w.write(i, address_marker(i));
                w.flush_word(i);
            }
            for i in 0..n {
                verify_word(w, base, i, address_marker(i), on_fault);
            }
        }
        RamTestPattern::MovingInversionsZeros => moving_inversions(w, base, 0, on_fault),
        RamTestPattern::MovingInversionsCheckerboard => {
            moving_inversions(w, base, PATTERN, on_fault);
        }
        RamTestPattern::WalkingOnes | RamTestPattern::WalkingZeros => {
            let zeros = matches!(pattern, RamTestPattern::WalkingZeros);
            for i in 0..n {
                w.write(i, walking_value(i, zeros));
                w.flush_word(i);
            }
            for i in 0..n {
                verify_word(w, base, i, walking_value(i, zeros), on_fault);
            }
        }
    }
}

/// Sum the frame-aligned bytes of the pre-selected usable `regions`, rounded
/// inward to whole frames exactly as [`sweep_pattern`] walks them, so a
/// progress callback can be given an honest denominator. A region the direct
/// map cannot reach is still counted here (the driver discovers
/// unreachability per window); an overflowing region contributes nothing,
/// matching the driver.
fn regions_frame_bytes(regions: &[MemoryRegion]) -> u64 {
    let mut total: u64 = 0;
    for region in regions {
        let Some(region_end) = region.end() else {
            continue;
        };
        let start = align_up(region.start.as_u64(), PAGE_SIZE as u64);
        let end = align_down(region_end.as_u64(), PAGE_SIZE as u64);
        if end > start {
            total = total.saturating_add(end - start);
        }
    }
    total
}

/// The total number of frame-aligned bytes one pattern sweep of `regions`
/// walks — the honest denominator for a progress fraction.
///
/// `regions` is the reserved-memory snapshot [`snapshot_free_regions`]
/// builds (currently-free RAM only, with any firmware-carved exclusion already
/// carved out). Computed once by the takeover driver before the loop begins and
/// passed to [`sweep_pattern`] as `total`, so the UI's percentage is stable
/// across the many patterns and loops rather than recomputed each time.
#[must_use]
pub fn takeover_test_bytes(regions: &[MemoryRegion]) -> u64 {
    regions_frame_bytes(regions)
}

/// Append the usable frame span `[start, start + len)` to `out[*n]`, unless
/// `len` is zero (nothing to add) or `out` is full (fail closed — the caller
/// refuses the takeover rather than sweep a truncated map). A written entry
/// is always [`RegionKind::Usable`]: the snapshot holds only sweepable RAM.
fn push_region(out: &mut [MemoryRegion], n: &mut usize, start: u64, len: u64) -> bool {
    if len == 0 {
        return true;
    }
    if *n >= out.len() {
        return false;
    }
    out[*n] = MemoryRegion {
        start: PhysAddr::new(start),
        length: len,
        kind: RegionKind::Usable,
    };
    *n += 1;
    true
}

/// The most exclusion ranges [`snapshot_free_regions`] will carve out of the
/// sweep in one call.
///
/// The sweep already tests only *free* RAM (the frame allocator marks every
/// in-use frame used), so the excludes cover only ranges the allocator does
/// **not** know are in use: a firmware-carved framebuffer that sits in usable
/// DRAM the allocator may still consider free. That is one range per active
/// scan-out surface, so this cap is generously above any real count; more raw
/// excludes than this is a programming error and is refused fail-closed.
pub const MAX_SWEEP_EXCLUDES: usize = 8;

/// Normalise `excludes` into `[start, end)` half-open intervals in `norm`,
/// dropping zero-length entries, then sort and merge overlapping/adjacent
/// ones so the subtraction below walks a clean, ascending, disjoint set.
/// Returns the number of merged intervals, or [`None`] if there are more raw
/// excludes than `norm` holds (fail closed — the caller refuses the takeover
/// rather than sweep a region it should keep out).
fn normalise_excludes(
    excludes: &[(PhysAddr, u64)],
    norm: &mut [(u64, u64); MAX_SWEEP_EXCLUDES],
) -> Option<usize> {
    let mut m = 0usize;
    for &(base, len) in excludes {
        if len == 0 {
            continue;
        }
        if m >= norm.len() {
            return None;
        }
        let s = base.as_u64();
        norm[m] = (s, s.saturating_add(len));
        m += 1;
    }
    // Insertion sort by start — `m` is small (a framebuffer or two), so an
    // in-place O(m^2) sort with no allocation is right here.
    for i in 1..m {
        let cur = norm[i];
        let mut j = i;
        while j > 0 && norm[j - 1].0 > cur.0 {
            norm[j] = norm[j - 1];
            j -= 1;
        }
        norm[j] = cur;
    }
    // Merge overlapping or touching intervals in place.
    let mut w = 0usize;
    for r in 1..m {
        let (rs, re) = norm[r];
        if rs <= norm[w].1 {
            if re > norm[w].1 {
                norm[w].1 = re;
            }
        } else {
            w += 1;
            norm[w] = (rs, re);
        }
    }
    Some(if m == 0 { 0 } else { w + 1 })
}

/// Emit the region `[start, end)` into `out`, minus every span in the
/// pre-normalised (ascending, disjoint) `excl` set: walk it left to right,
/// pushing each gap before the next overlapping exclude and skipping the
/// excluded spans. Returns `false` if `out` filled mid-region (the caller
/// decides whether that is fail-closed or a graceful truncation).
fn emit_region_minus_excludes(
    start: u64,
    end: u64,
    excl: &[(u64, u64)],
    out: &mut [MemoryRegion],
    n: &mut usize,
) -> bool {
    let mut cursor = start;
    for &(es, ee) in excl {
        if ee <= cursor || es >= end {
            // This exclude is wholly before the cursor or wholly past the
            // region; it carves nothing here.
            continue;
        }
        if es > cursor && !push_region(out, n, cursor, es - cursor) {
            return false;
        }
        // Advance past the excluded span (clamped to the region end).
        cursor = cursor.max(ee.min(end));
    }
    if cursor < end && !push_region(out, n, cursor, end - cursor) {
        return false;
    }
    true
}

/// Copy the frame allocator's currently-**free** physical runs into `out` — a
/// **reserved-memory snapshot** the takeover sweep walks — carving out every
/// `excludes` range.
///
/// This is the sweep target for the one-way whole-RAM `memtest`
/// (`plans/NEW-SUPERVISOR.md` §9). The free-run set
/// ([`FrameAllocator::for_each_free_region`]) is the single authority on which
/// RAM is safe to overwrite: it already excludes **every** frame the running
/// system holds — the kernel image and page tables, the heap (the takeover's
/// own console cell grids and audit ring included), DMA buffers a device may
/// still map non-cacheably, and all driver and userland memory — because the
/// allocator marks them used. Writing such an in-use frame races its owner and
/// can wedge the machine; that is exactly what froze a real Raspberry Pi 4
/// (the sweep reached a live, non-cacheable DMA buffer the old boot-map
/// snapshot still called "usable"). Testing only free RAM is the honest
/// maximum-safe coverage, exactly as a running memtest86 cannot test its own
/// resident working set.
///
/// `excludes` adds any range the *allocator* does not already know is in use —
/// notably a firmware-carved framebuffer that sits in usable DRAM the
/// allocator may still consider free — so the live scan-out surface survives
/// the run. Overlapping or adjacent excludes are merged; a free run is split
/// into the sub-ranges outside every exclude, and one wholly inside an exclude
/// is dropped.
///
/// The snapshot is read once, before any write, into caller-owned reserved
/// memory (the reserved takeover stack); the sweep thereafter reads only the
/// snapshot and never the heap-backed structures it is about to overwrite.
///
/// Returns the number of regions written, or [`None`] only when there are more
/// raw `excludes` than [`MAX_SWEEP_EXCLUDES`] (a programming error, fail
/// closed). Because every free run is inherently safe to sweep, a set that
/// would overflow `out` is **truncated** — the surplus free runs simply go
/// untested this loop (the test loops forever) — never refused and never
/// causing an in-use frame to be swept.
pub fn snapshot_free_regions(
    frames: &FrameAllocator,
    excludes: &[(PhysAddr, u64)],
    out: &mut [MemoryRegion],
) -> Option<usize> {
    let mut norm = [(0u64, 0u64); MAX_SWEEP_EXCLUDES];
    let ex_count = normalise_excludes(excludes, &mut norm)?;
    let excl = &norm[..ex_count];

    let mut n = 0usize;
    let mut full = false;
    frames.for_each_free_region(|base, len| {
        if full {
            return;
        }
        let start = base.as_u64();
        let Some(end) = start.checked_add(len) else {
            return;
        };
        if !emit_region_minus_excludes(start, end, excl, out, &mut n) {
            // `out` is full: stop appending. The runs already written are a
            // safe (free-only) subset; the rest go untested this loop.
            full = true;
        }
    });
    Some(n)
}

/// The two things a [`sweep_pattern`] run reports as it goes: forward
/// progress and each detected fault.
///
/// A single observer is threaded through the whole sweep, so the driver's
/// live display (progress bar + scrolling fault log) is updated through one
/// borrow — the progress and fault paths share the UI without a second
/// mutable alias. The two methods are never called at the same instant (the
/// sweep tests a window, then reports its progress), so an implementation may
/// freely mutate shared state in both.
pub trait SweepObserver {
    /// Called with the physical base address of each window *before* it is
    /// tested, so a UI can show exactly which physical page is under test —
    /// the last value shown names the frame the sweep was on if a bad DRAM
    /// cell (or a wedged access) ever stalls the run. The default is a no-op
    /// for observers that do not surface it.
    fn window(&mut self, _phys: u64) {}
    /// Called after each window with the cumulative bytes swept in *this*
    /// pattern and the precomputed `total` of all reachable frame-aligned
    /// usable bytes, so a UI can render a fraction.
    fn progress(&mut self, tested: u64, total: u64);
    /// Called for **every** cell that fails to read back; the sweep never
    /// stops early, so one bad cell does not mask the rest.
    fn fault(&mut self, fault: RamFault);
}

/// Apply one [`RamTestPattern`] to **every** word of every reachable region
/// in `regions`, reporting progress and every fault through `observer`.
///
/// `regions` is the reserved-memory snapshot [`snapshot_free_regions`]
/// builds: currently-**free** RAM only, with any firmware-carved exclusion
/// (the console framebuffer) already carved out. This is the engine behind the
/// Supervisor's `memtest` takeover (`plans/NEW-SUPERVISOR.md` §9): once the
/// machine has been quiesced and handed to the test it exercises all free
/// RAM. Every in-use frame — a spawned process's memory, a DMA buffer a device
/// still maps non-cacheably, the kernel heap the takeover itself renders
/// through — is *not* in `regions`, because the frame allocator marks it used;
/// writing one would race its owner and can wedge the machine. It therefore
/// **overwrites and does not restore** the
/// memory it tests; the machine cannot resume and the only sequel is a reset.
/// The safety argument that lets [`run`] run pre-allocator does not apply —
/// the caller must have already taken the machine over (masked interrupts,
/// stopped the watchdog, quiesced the other CPUs) and must have kept out of
/// `regions` every frame the still-running takeover itself depends on (the
/// framebuffer it displays through, and — by building `regions` in reserved
/// memory — the region list itself).
///
/// The driver calls this once per pattern in [`RamTestPattern::ALL`] and
/// repeats the whole cycle until the machine is reset, so this function tests
/// one pattern over all of `regions` and returns; it never loops or resets
/// itself.
///
/// Each window's physical base is reported through [`SweepObserver::window`]
/// before it is tested, so a UI can name the exact frame under test (the last
/// one shown pins where the sweep was if a bad cell or a wedged access ever
/// stalls it). A window the direct map cannot reach is left untested and
/// uncounted rather than faked as tested (fail closed), exactly as in
/// [`run`].
pub fn sweep_pattern<M>(
    regions: &[MemoryRegion],
    physmap: &M,
    pattern: RamTestPattern,
    total: u64,
    observer: &mut dyn SweepObserver,
) where
    M: PhysMap + ?Sized,
{
    let mut tested: u64 = 0;
    for region in regions {
        let Some(region_end) = region.end() else {
            continue;
        };
        let start = align_up(region.start.as_u64(), PAGE_SIZE as u64);
        let end = align_down(region_end.as_u64(), PAGE_SIZE as u64);
        let mut addr = start;
        while addr < end {
            let remaining = end - addr;
            let chunk = remaining.min(PROGRESS_STEP_BYTES as u64);
            let Ok(chunk_len) = usize::try_from(chunk) else {
                break;
            };
            observer.window(addr);
            if let Some(window) = PhysWindow::new(physmap, PhysAddr::new(addr), chunk_len) {
                sweep_window(&window, PhysAddr::new(addr), pattern, &mut |fault| {
                    observer.fault(fault);
                });
                tested += chunk;
                observer.progress(tested, total);
            }
            addr += chunk;
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::phys::SimPhysMap;

    extern crate std;
    use core::cell::Cell;
    use std::vec::Vec;

    /// The usable regions of `map` as a slice the slice-based
    /// [`sweep_pattern`]/[`takeover_test_bytes`] take. The sweep rounds each
    /// region inward to whole frames itself, so the raw usable regions are the
    /// right input for a test that only wants "all of it".
    fn usable_slice(map: &BootMemoryMap) -> Vec<MemoryRegion> {
        map.regions()
            .iter()
            .filter(|r| r.kind == RegionKind::Usable)
            .copied()
            .collect()
    }

    /// A frame allocator over a single usable region of `frames` whole frames
    /// starting at `base`, with every frame initially free — the starting
    /// point for the free-run snapshot tests.
    fn free_alloc(base: u64, frames: usize) -> FrameAllocator {
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            start: PhysAddr::new(base),
            length: frames as u64 * PAGE_SIZE as u64,
            kind: RegionKind::Usable,
        });
        FrameAllocator::new(&map).expect("valid usable map")
    }

    /// How a [`FakeRam`] misbehaves, so the passes can be shown to catch each
    /// class of fault.
    #[derive(Clone, Copy)]
    enum Fault {
        /// Healthy RAM.
        None,
        /// Data bit `bit` of word `word` is stuck at zero.
        StuckLow { word: usize, bit: u32 },
        /// Data bit `bit` of word `word` is stuck at one.
        StuckHigh { word: usize, bit: u32 },
        /// Accesses to word `from` land on word `to` (a shorted address line).
        Alias { from: usize, to: usize },
    }

    /// A host-memory [`WordWindow`] that can inject a fault, for exercising
    /// the engine without real hardware.
    struct FakeRam {
        cells: Vec<Cell<Word>>,
        fault: Fault,
        flushes: Cell<usize>,
    }

    impl FakeRam {
        fn new(words: usize, fault: Fault) -> Self {
            let mut cells = Vec::with_capacity(words);
            for _ in 0..words {
                cells.push(Cell::new(0));
            }
            Self {
                cells,
                fault,
                flushes: Cell::new(0),
            }
        }

        /// Redirect an index through an aliasing fault, if any.
        fn index(&self, i: usize) -> usize {
            match self.fault {
                Fault::Alias { from, to } if i == from => to,
                _ => i,
            }
        }
    }

    impl WordWindow for FakeRam {
        fn words(&self) -> usize {
            self.cells.len()
        }

        fn read(&self, i: usize) -> Word {
            self.cells[self.index(i)].get()
        }

        fn write(&self, i: usize, value: Word) {
            let j = self.index(i);
            let stored = match self.fault {
                Fault::StuckLow { word, bit } if j == word => value & !(1 << bit),
                Fault::StuckHigh { word, bit } if j == word => value | (1 << bit),
                _ => value,
            };
            self.cells[j].set(stored);
        }

        fn flush_word(&self, _i: usize) {
            self.flushes.set(self.flushes.get() + 1);
        }
    }

    /// One word sampled per this many by the device pass, mirrored in the
    /// tests so a fault can be placed on (or deliberately between) samples.
    const STRIDE: usize = DEVICE_SAMPLE_STRIDE_WORDS;

    #[test]
    fn healthy_window_passes_and_is_left_zeroed() {
        let ram = FakeRam::new(64, Fault::None);
        assert_eq!(test_window(&ram, PhysAddr::new(0x4000)), Ok(()));
        assert!(ram.cells.iter().all(|c| c.get() == 0));
        // Every pass flushed at least once, so a read can never be served a
        // stale cached value.
        assert!(ram.flushes.get() >= 3);
    }

    #[test]
    fn a_stuck_low_bit_at_an_odd_position_is_caught_by_the_pattern_sweep() {
        // The device pass drives sampled word 0 to `PATTERN` (0xAAAA…, every
        // odd bit set), so a bit stuck low at an odd position (bit 9)
        // mismatches on the `PATTERN` read-back.
        let ram = FakeRam::new(STRIDE, Fault::StuckLow { word: 0, bit: 9 });
        let fault = test_window(&ram, PhysAddr::new(0x1_0000)).unwrap_err();
        assert_eq!(fault.phys, PhysAddr::new(0x1_0000));
        // The one differing bit is exactly the stuck one.
        assert_eq!(fault.expected ^ fault.observed, 1 << 9);
    }

    #[test]
    fn a_stuck_low_bit_at_an_even_position_is_caught_by_the_antipattern_sweep() {
        // `ANTIPATTERN` (0x5555…, every even bit set) catches a bit stuck low
        // at an even position (bit 4), the polarity `PATTERN` alone would
        // have read back as correct.
        let ram = FakeRam::new(STRIDE, Fault::StuckLow { word: 0, bit: 4 });
        let fault = test_window(&ram, PhysAddr::new(0x1_0000)).unwrap_err();
        assert_eq!(fault.phys, PhysAddr::new(0x1_0000));
        assert_eq!(fault.expected ^ fault.observed, 1 << 4);
    }

    #[test]
    fn a_stuck_high_bit_is_caught_by_the_complementary_pattern() {
        // A bit stuck at one is set where a pattern wants a zero: bit 6 is
        // even, so `PATTERN` (odd bits) expects it low and the stuck-high
        // sampled cell reads back with bit 6 set — caught with no all-zero
        // sweep needed.
        let ram = FakeRam::new(STRIDE, Fault::StuckHigh { word: 0, bit: 6 });
        let fault = test_window(&ram, PhysAddr::new(0x1_0000)).unwrap_err();
        assert_eq!(fault.phys, PhysAddr::new(0x1_0000));
        assert_eq!(fault.expected ^ fault.observed, 1 << 6);
    }

    #[test]
    fn the_device_pass_samples_the_second_page_not_only_the_first() {
        // A stuck bit in the sampled word of the *second* page (word
        // `STRIDE`) proves the sample advances by the stride across the whole
        // window, not only testing word 0.
        let ram = FakeRam::new(
            STRIDE + 1,
            Fault::StuckLow {
                word: STRIDE,
                bit: 1,
            },
        );
        let fault = test_window(&ram, PhysAddr::new(0x1_0000)).unwrap_err();
        assert_eq!(
            fault.phys,
            PhysAddr::new(0x1_0000 + (STRIDE as u64) * WORD_BYTES as u64)
        );
        assert_eq!(fault.expected ^ fault.observed, 1 << 1);
    }

    #[test]
    fn a_single_bit_fault_between_two_samples_is_the_accepted_sampling_gap() {
        // Word 5 is neither a power-of-two offset (so the address pass never
        // writes it) nor a multiple of the stride (so the device pass never
        // samples it): a lone stuck bit there is not caught. This is the
        // coverage the sampling test deliberately trades for finishing in a
        // couple of seconds on many gigabytes — documented, not accidental.
        let ram = FakeRam::new(STRIDE, Fault::StuckLow { word: 5, bit: 0 });
        assert_eq!(test_window(&ram, PhysAddr::new(0x1_0000)), Ok(()));
    }

    #[test]
    fn a_shorted_address_line_is_caught_by_the_address_pass() {
        // Writes to word 2 land on word 4, so word 2's marker never reaches
        // word 2 (word 4's later write wins there): reading word 2 back
        // observes the wrong marker and the pass fails closed naming word 2,
        // the offset it was reading.
        let ram = FakeRam::new(8, Fault::Alias { from: 2, to: 4 });
        let fault = test_window(&ram, PhysAddr::new(0x2000)).unwrap_err();
        assert_eq!(fault.phys, PhysAddr::new(0x2000 + 2 * WORD_BYTES as u64));
    }

    #[test]
    fn a_single_word_window_still_proves_its_lone_cell() {
        let ram = FakeRam::new(1, Fault::None);
        assert_eq!(test_window(&ram, PhysAddr::new(0x8000)), Ok(()));
        let stuck = FakeRam::new(1, Fault::StuckLow { word: 0, bit: 0 });
        assert!(test_window(&stuck, PhysAddr::new(0x8000)).is_err());
    }

    #[test]
    fn align_helpers_round_the_right_way() {
        assert_eq!(align_up(1, 4096), 4096);
        assert_eq!(align_up(4096, 4096), 4096);
        assert_eq!(align_down(4097, 4096), 4096);
        assert_eq!(align_down(4095, 4096), 0);
        // The upward round saturates instead of wrapping at the top.
        assert_eq!(align_up(u64::MAX, 4096), !4095u64);
    }

    /// Build a one-region usable map and drive [`run`] over a [`SimPhysMap`],
    /// proving the production `PhysWindow` accessor's pointer arithmetic and
    /// the whole-map driver on the host, and that the passes leave no test
    /// pattern behind (every cell they wrote is restored to zero).
    #[test]
    fn run_tests_the_usable_region_over_the_direct_map_and_leaves_no_pattern() {
        let base = 0x10_0000u64;
        let len = 3 * PAGE_SIZE;
        let sim = SimPhysMap::new(PhysAddr::new(base), len);
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            start: PhysAddr::new(base),
            length: len as u64,
            kind: RegionKind::Usable,
        });

        let mut steps = 0usize;
        let mut last = 0u64;
        let tested = run(&map, &sim, |cumulative| {
            steps += 1;
            last = cumulative;
        })
        .expect("healthy simulated RAM passes");
        assert_eq!(tested, len as u64);
        assert_eq!(last, len as u64);
        assert_eq!(steps, 1, "a sub-step region reports progress once");

        // Every cell the passes wrote is restored to zero, and the simulator
        // starts zeroed, so no test pattern is left behind — checked
        // byte-wise so the test needs no aligned-pointer cast of its own.
        let ptr = sim
            .translate(PhysAddr::new(base), len)
            .expect("mapped")
            .as_ptr();
        for i in 0..len {
            // SAFETY: `[base, base+len)` is the simulator's own allocation and
            // `i` is in range; the read observes the zeroed byte.
            let v = unsafe { ptr.add(i).read() };
            assert_eq!(v, 0);
        }
    }

    #[test]
    fn test_owned_window_tests_a_mapped_window_and_leaves_it_zeroed() {
        let base = 0x40_0000u64;
        let len = PAGE_SIZE;
        let sim = SimPhysMap::new(PhysAddr::new(base), len);
        assert_eq!(
            test_owned_window(&sim, PhysAddr::new(base), len),
            Ok(true),
            "healthy owned window passes"
        );
        // The passes restore every cell they wrote to zero.
        let ptr = sim
            .translate(PhysAddr::new(base), len)
            .expect("mapped")
            .as_ptr();
        for i in 0..len {
            // SAFETY: `[base, base+len)` is the simulator's own allocation and
            // `i` is in range.
            assert_eq!(unsafe { ptr.add(i).read() }, 0);
        }
    }

    #[test]
    fn test_owned_window_reports_an_unmappable_window_as_skipped_not_a_pass() {
        // The simulator covers one span; a window far outside it is not
        // mappable, so the test is skipped (`Ok(false)`) rather than trusted.
        let base = 0x50_0000u64;
        let sim = SimPhysMap::new(PhysAddr::new(base), PAGE_SIZE);
        assert_eq!(
            test_owned_window(&sim, PhysAddr::new(base + 0x100_0000), PAGE_SIZE),
            Ok(false),
        );
    }

    #[test]
    fn run_skips_reserved_regions_and_counts_only_usable_bytes() {
        let base = 0x20_0000u64;
        let len = 2 * PAGE_SIZE;
        let sim = SimPhysMap::new(PhysAddr::new(base), len);
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            start: PhysAddr::new(base),
            length: PAGE_SIZE as u64,
            kind: RegionKind::Reserved,
        });
        map.push(MemoryRegion {
            start: PhysAddr::new(base + PAGE_SIZE as u64),
            length: PAGE_SIZE as u64,
            kind: RegionKind::Usable,
        });
        let tested = run(&map, &sim, |_| {}).expect("healthy");
        assert_eq!(tested, PAGE_SIZE as u64);
    }

    #[test]
    fn run_leaves_an_unmappable_region_untested_rather_than_trusting_it() {
        // The simulator covers one region; a second usable region lies far
        // outside it. The reachable region is tested; the unreachable one is
        // skipped (uncounted), never faked as a pass.
        let base = 0x30_0000u64;
        let sim = SimPhysMap::new(PhysAddr::new(base), PAGE_SIZE);
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            start: PhysAddr::new(base),
            length: PAGE_SIZE as u64,
            kind: RegionKind::Usable,
        });
        map.push(MemoryRegion {
            start: PhysAddr::new(base + 0x100_0000),
            length: PAGE_SIZE as u64,
            kind: RegionKind::Usable,
        });
        let tested = run(&map, &sim, |_| {}).expect("reachable region passes");
        assert_eq!(tested, PAGE_SIZE as u64);
    }

    /// Collect every fault a sweep of `ram` with `pattern` reports.
    fn faults_of(ram: &FakeRam, base: u64, pattern: RamTestPattern) -> Vec<RamFault> {
        let mut faults = Vec::new();
        sweep_window(ram, PhysAddr::new(base), pattern, &mut |f| faults.push(f));
        faults
    }

    /// A [`SweepObserver`] that records the last progress figures, the last
    /// window base reported, and every fault, for driving [`sweep_pattern`] on
    /// the host.
    #[derive(Default)]
    struct RecordingObserver {
        last_tested: u64,
        last_total: u64,
        last_window: Option<u64>,
        windows: u64,
        faults: Vec<RamFault>,
    }

    impl SweepObserver for RecordingObserver {
        fn window(&mut self, phys: u64) {
            self.last_window = Some(phys);
            self.windows += 1;
        }
        fn progress(&mut self, tested: u64, total: u64) {
            self.last_tested = tested;
            self.last_total = total;
        }
        fn fault(&mut self, fault: RamFault) {
            self.faults.push(fault);
        }
    }

    #[test]
    fn checkerboard_sweep_touches_every_word_and_leaves_the_pattern_behind() {
        // The takeover sweep is full-coverage and one-way: unlike
        // `test_window`, it writes *every* cell and never restores it. A
        // healthy window reports no fault, and the checkerboard
        // moving-inversions pattern ends with every word holding `PATTERN`
        // (its descending pass writes the value back), so every word was
        // written and nothing was zeroed.
        let words = 3 * STRIDE + 7;
        let ram = FakeRam::new(words, Fault::None);
        assert!(faults_of(&ram, 0x4000, RamTestPattern::MovingInversionsCheckerboard).is_empty());
        assert!(
            ram.cells.iter().all(|c| c.get() == PATTERN),
            "every word is left holding the final moving-inversions pattern"
        );
    }

    #[test]
    fn own_address_sweep_catches_a_stuck_bit_between_the_sampling_gaps() {
        // Word 5 is neither a power-of-two offset nor a stride multiple, so
        // the *sampling* `test_window` deliberately misses a lone fault there
        // — the full-coverage own-address sweep must not: it tests every word.
        let ram = FakeRam::new(STRIDE, Fault::StuckLow { word: 5, bit: 0 });
        let faults = faults_of(&ram, 0x1_0000, RamTestPattern::OwnAddress);
        assert!(faults
            .iter()
            .any(|f| f.phys == PhysAddr::new(0x1_0000 + 5 * WORD_BYTES as u64)));
    }

    #[test]
    fn moving_inversions_catches_a_stuck_high_bit_naming_the_cell() {
        let ram = FakeRam::new(16, Fault::StuckHigh { word: 9, bit: 12 });
        let faults = faults_of(&ram, 0x2000, RamTestPattern::MovingInversionsZeros);
        let fault = faults
            .iter()
            .find(|f| f.phys == PhysAddr::new(0x2000 + 9 * WORD_BYTES as u64))
            .expect("the stuck-high cell is reported");
        assert_ne!(fault.expected & (1 << 12), fault.observed & (1 << 12));
    }

    #[test]
    fn own_address_sweep_catches_a_shorted_address_line() {
        // Writes to word 2 land on word 5; reading word 2 back observes word
        // 5's marker, so the address-in-address pass reports word 2 (the
        // offset it was reading).
        let ram = FakeRam::new(8, Fault::Alias { from: 2, to: 5 });
        let faults = faults_of(&ram, 0x8000, RamTestPattern::OwnAddress);
        assert!(faults
            .iter()
            .any(|f| f.phys == PhysAddr::new(0x8000 + 2 * WORD_BYTES as u64)));
    }

    #[test]
    fn a_sweep_reports_the_bad_cell_without_stopping_the_rest() {
        // A stuck bit is reported through the callback while the sweep keeps
        // going: the moving-inversions pass over word 7 (which the all-ones
        // half drives high) sees bit 3 stuck low and reports it, and the
        // ascending + descending passes both still run to completion over the
        // whole window (proved by the fault appearing in the report at all —
        // the callback path never returns early).
        let ram = FakeRam::new(4 * STRIDE, Fault::StuckLow { word: 7, bit: 3 });
        let faults = faults_of(&ram, 0, RamTestPattern::MovingInversionsZeros);
        assert!(faults
            .iter()
            .any(|f| f.phys == PhysAddr::new(7 * WORD_BYTES as u64)));
    }

    #[test]
    fn sweep_pattern_covers_a_healthy_map_and_does_not_restore_it() {
        let base = 0x10_0000u64;
        let len = 2 * PAGE_SIZE;
        let sim = SimPhysMap::new(PhysAddr::new(base), len);
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            start: PhysAddr::new(base),
            length: len as u64,
            kind: RegionKind::Usable,
        });

        let regions = usable_slice(&map);
        let total = takeover_test_bytes(&regions);
        assert_eq!(
            total, len as u64,
            "the honest denominator is all usable RAM"
        );
        let mut observer = RecordingObserver::default();
        sweep_pattern(
            &regions,
            &sim,
            RamTestPattern::MovingInversionsCheckerboard,
            total,
            &mut observer,
        );
        assert!(
            observer.faults.is_empty(),
            "healthy simulated RAM reports no fault"
        );
        assert_eq!(observer.last_tested, len as u64);
        assert_eq!(observer.last_total, len as u64);
        // The first window's physical base is reported through `window`.
        assert_eq!(observer.last_window, Some(base));
        assert!(observer.windows >= 1, "each window is announced");

        // The takeover sweep never restores: the RAM is left holding the
        // final pattern, not zeroed (checked byte-wise, no aligned cast).
        let ptr = sim
            .translate(PhysAddr::new(base), len)
            .expect("mapped")
            .as_ptr();
        let pattern_bytes = PATTERN.to_ne_bytes();
        for i in 0..len {
            // SAFETY: `[base, base+len)` is the simulator's own allocation and
            // `i` is in range.
            let v = unsafe { ptr.add(i).read() };
            assert_eq!(v, pattern_bytes[i % WORD_BYTES]);
        }
    }

    #[test]
    fn sweep_pattern_skips_an_unmappable_region_rather_than_faking_it() {
        // The simulator covers one region; a second usable region lies far
        // outside it. The reachable region is swept (its bytes counted); the
        // unreachable one is skipped, never faked as tested.
        let base = 0x30_0000u64;
        let sim = SimPhysMap::new(PhysAddr::new(base), PAGE_SIZE);
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            start: PhysAddr::new(base),
            length: PAGE_SIZE as u64,
            kind: RegionKind::Usable,
        });
        map.push(MemoryRegion {
            start: PhysAddr::new(base + 0x100_0000),
            length: PAGE_SIZE as u64,
            kind: RegionKind::Usable,
        });
        let regions = usable_slice(&map);
        let total = takeover_test_bytes(&regions);
        let mut observer = RecordingObserver::default();
        sweep_pattern(
            &regions,
            &sim,
            RamTestPattern::WalkingZeros,
            total,
            &mut observer,
        );
        assert!(observer.faults.is_empty(), "healthy RAM reports no fault");
        assert_eq!(
            observer.last_tested, PAGE_SIZE as u64,
            "only the reachable region"
        );
    }

    #[test]
    fn every_pattern_has_a_distinct_display_name() {
        let mut names = Vec::new();
        for p in RamTestPattern::ALL {
            let name = p.name();
            assert!(!name.is_empty());
            assert!(!names.contains(&name), "names are distinct");
            names.push(name);
        }
        assert_eq!(names.len(), RamTestPattern::ALL.len());
    }

    #[test]
    fn takeover_test_bytes_counts_only_frame_aligned_usable_span() {
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            start: PhysAddr::new(0x1000),
            length: 2 * PAGE_SIZE as u64,
            kind: RegionKind::Usable,
        });
        map.push(MemoryRegion {
            start: PhysAddr::new(0x100_0000),
            length: PAGE_SIZE as u64,
            kind: RegionKind::Reserved,
        });
        // The snapshot keeps only the usable region; the reserved one is
        // dropped, and the count is its whole frame-aligned span.
        let regions = usable_slice(&map);
        assert_eq!(regions.len(), 1);
        assert_eq!(takeover_test_bytes(&regions), 2 * PAGE_SIZE as u64);
    }

    #[test]
    fn snapshot_excludes_frames_the_allocator_handed_out() {
        // The core Raspberry Pi 4 regression: a frame the allocator has handed
        // out (a live DMA buffer, say) must never appear in the sweep's
        // free-run snapshot, even though the boot map still calls its whole
        // region "usable". Overwriting such an in-use, possibly non-cacheably
        // mapped frame is what wedged the board.
        let base = 0x10_0000u64;
        let p = PAGE_SIZE as u64;
        let frames = free_alloc(base, 8);
        // Hand out four frames, then return all but one so a single in-use
        // frame sits amid otherwise-free RAM, exactly like a DMA buffer.
        let f: Vec<_> = (0..4)
            .map(|_| frames.alloc().expect("free frame"))
            .collect();
        frames.free(f[0]).expect("free");
        frames.free(f[1]).expect("free");
        frames.free(f[3]).expect("free");
        let held = f[2].start().as_u64();

        let mut buf = [MemoryRegion {
            start: PhysAddr::new(0),
            length: 0,
            kind: RegionKind::Usable,
        }; 8];
        let n = snapshot_free_regions(&frames, &[], &mut buf).expect("no excludes");
        // The held frame is covered by none of the reported free runs.
        for region in &buf[..n] {
            let start = region.start.as_u64();
            let end = start + region.length;
            assert!(
                held < start || held >= end,
                "the in-use frame {held:#x} must be excluded from the sweep"
            );
        }
        // Exactly the one held frame is kept out; every other usable frame is
        // tested.
        assert_eq!(takeover_test_bytes(&buf[..n]), 7 * p);
    }

    #[test]
    fn snapshot_carves_an_interior_exclusion_into_two_regions() {
        // A free run with the framebuffer sitting in its middle is split into
        // the two sub-ranges either side of the excluded span, so the sweep
        // never writes the live scan-out surface.
        let base = 0x10_0000u64;
        let len = 8 * PAGE_SIZE as u64;
        let frames = free_alloc(base, 8);
        // Exclude the middle two frames.
        let fb_base = base + 3 * PAGE_SIZE as u64;
        let fb_len = 2 * PAGE_SIZE as u64;
        let mut buf = [MemoryRegion {
            start: PhysAddr::new(0),
            length: 0,
            kind: RegionKind::Usable,
        }; 8];
        let n = snapshot_free_regions(&frames, &[(PhysAddr::new(fb_base), fb_len)], &mut buf)
            .expect("fits");
        assert_eq!(n, 2, "a straddled run splits into two");
        assert_eq!(buf[0].start, PhysAddr::new(base));
        assert_eq!(buf[0].length, 3 * PAGE_SIZE as u64);
        assert_eq!(buf[1].start, PhysAddr::new(fb_base + fb_len));
        assert_eq!(buf[1].length, len - 5 * PAGE_SIZE as u64);
        // The excluded span contributes no tested bytes.
        assert_eq!(takeover_test_bytes(&buf[..n]), len - fb_len);
    }

    #[test]
    fn snapshot_drops_a_run_wholly_inside_the_exclusion() {
        // Two disjoint free runs (a non-usable gap between them is never free)
        // with the exclusion covering the first: the covered run is dropped and
        // the disjoint one passes through untouched.
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            start: PhysAddr::new(0x20_0000),
            length: PAGE_SIZE as u64,
            kind: RegionKind::Usable,
        });
        map.push(MemoryRegion {
            start: PhysAddr::new(0x40_0000),
            length: PAGE_SIZE as u64,
            kind: RegionKind::Usable,
        });
        let frames = FrameAllocator::new(&map).expect("valid map");
        let mut buf = [MemoryRegion {
            start: PhysAddr::new(0),
            length: 0,
            kind: RegionKind::Usable,
        }; 8];
        let n = snapshot_free_regions(
            &frames,
            &[(PhysAddr::new(0x20_0000), PAGE_SIZE as u64)],
            &mut buf,
        )
        .expect("fits");
        assert_eq!(n, 1);
        assert_eq!(buf[0].start, PhysAddr::new(0x40_0000));
    }

    #[test]
    fn snapshot_carves_several_out_of_order_excludes_from_one_run() {
        // A single free run with three firmware-carved excludes (given out of
        // order and one touching another) is split into exactly the gaps
        // between them, proving the sort/merge + multi-range subtraction.
        let base = 0x10_0000u64;
        let len = 16 * PAGE_SIZE as u64;
        let frames = free_alloc(base, 16);
        let p = PAGE_SIZE as u64;
        // Frames [3,4) and [4,5) (adjacent → merge) and [10,12), given out of
        // order to exercise the sort.
        let excludes = [
            (PhysAddr::new(base + 10 * p), 2 * p),
            (PhysAddr::new(base + 3 * p), p),
            (PhysAddr::new(base + 4 * p), p),
        ];
        let mut buf = [MemoryRegion {
            start: PhysAddr::new(0),
            length: 0,
            kind: RegionKind::Usable,
        }; 8];
        let n = snapshot_free_regions(&frames, &excludes, &mut buf).expect("fits");
        assert_eq!(n, 3, "three gaps: [0,3), [5,10), [12,16)");
        assert_eq!((buf[0].start, buf[0].length), (PhysAddr::new(base), 3 * p));
        assert_eq!(
            (buf[1].start, buf[1].length),
            (PhysAddr::new(base + 5 * p), 5 * p)
        );
        assert_eq!(
            (buf[2].start, buf[2].length),
            (PhysAddr::new(base + 12 * p), 4 * p)
        );
        // Total tested = run minus the four excluded frames.
        assert_eq!(takeover_test_bytes(&buf[..n]), len - 4 * p);
    }

    #[test]
    fn snapshot_fails_closed_on_too_many_excludes() {
        // More raw excludes than MAX_SWEEP_EXCLUDES is a programming error,
        // refused fail-closed, so the takeover never sweeps a range it was
        // asked to keep out.
        let frames = free_alloc(0x10_0000, 0x1000);
        let mut excludes = Vec::new();
        for i in 0..=(MAX_SWEEP_EXCLUDES as u64) {
            excludes.push((PhysAddr::new(0x10_0000 + i * 0x2000), PAGE_SIZE as u64));
        }
        let mut buf = [MemoryRegion {
            start: PhysAddr::new(0),
            length: 0,
            kind: RegionKind::Usable,
        }; 32];
        assert_eq!(snapshot_free_regions(&frames, &excludes, &mut buf), None);
    }

    #[test]
    fn snapshot_truncates_when_the_buffer_is_too_small() {
        // Every free run is inherently safe to sweep, so more free runs than
        // the buffer holds are truncated — the surplus simply go untested this
        // loop — rather than refused. Only an in-use frame must never be swept,
        // and truncation only ever *omits* free RAM, never adds any.
        let base = 0x10_0000u64;
        let p = PAGE_SIZE as u64;
        let frames = free_alloc(base, 8);
        // Hand out every frame, then return the ones at base+{0,2,4,6} (chosen
        // by address, not allocation order) so the free set is four isolated
        // single-frame runs.
        let f: Vec<_> = (0..8)
            .map(|_| frames.alloc().expect("free frame"))
            .collect();
        for &k in &[0u64, 2, 4, 6] {
            let target = base + k * p;
            let fr = *f
                .iter()
                .find(|fr| fr.start().as_u64() == target)
                .expect("frame handed out");
            frames.free(fr).expect("free");
        }
        let mut buf = [MemoryRegion {
            start: PhysAddr::new(0),
            length: 0,
            kind: RegionKind::Usable,
        }; 2];
        let n = snapshot_free_regions(&frames, &[], &mut buf).expect("truncates, not None");
        assert_eq!(n, 2, "the two-slot buffer holds the first two free runs");
        assert_eq!((buf[0].start, buf[0].length), (PhysAddr::new(base), p));
        assert_eq!(
            (buf[1].start, buf[1].length),
            (PhysAddr::new(base + 2 * p), p)
        );
    }
}
