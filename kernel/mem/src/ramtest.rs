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

use crate::bootinfo::{BootMemoryMap, RegionKind};
use crate::frame::{PhysAddr, PAGE_SIZE};
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

/// Sum the usable bytes of `map`, rounded inward to whole frames exactly as
/// [`sweep_pattern`] walks them, so a progress callback can be given an
/// honest denominator. A region the direct map cannot reach is still counted
/// here (the driver discovers unreachability per window); an overflowing
/// region is skipped, matching the driver.
fn usable_frame_bytes(map: &BootMemoryMap) -> u64 {
    let mut total: u64 = 0;
    for region in map.regions() {
        if region.kind != RegionKind::Usable {
            continue;
        }
        let Some(region_end) = region.end() else {
            continue;
        };
        let start = align_up(region.start.as_u64(), PAGE_SIZE as u64);
        let end = align_down(region_end.as_u64(), PAGE_SIZE as u64);
        if end > start {
            total += end - start;
        }
    }
    total
}

/// The total number of reachable, frame-aligned usable bytes one pattern
/// sweep of `map` walks — the honest denominator for a progress fraction.
///
/// Computed once by the takeover driver before the loop begins and passed to
/// [`sweep_pattern`] as `total`, so the UI's percentage is stable across the
/// many patterns and loops rather than recomputed each time.
#[must_use]
pub fn takeover_test_bytes(map: &BootMemoryMap) -> u64 {
    usable_frame_bytes(map)
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
    /// Called after each window with the cumulative bytes swept in *this*
    /// pattern and the precomputed `total` of all reachable frame-aligned
    /// usable bytes, so a UI can render a fraction.
    fn progress(&mut self, tested: u64, total: u64);
    /// Called for **every** cell that fails to read back; the sweep never
    /// stops early, so one bad cell does not mask the rest.
    fn fault(&mut self, fault: RamFault);
}

/// Apply one [`RamTestPattern`] to **every** word of every reachable usable
/// region of `map`, reporting progress and every fault through `observer`.
///
/// This is the engine behind the Supervisor's `memtest` takeover
/// (`plans/NEW-SUPERVISOR.md` §9): once the machine has been quiesced and
/// handed to the test it exercises all of RAM — including the frames the live
/// kernel image, heap, page tables, and stacks occupied, which the read-only
/// [`run`]/[`test_owned_window`] paths can never touch. It therefore
/// **overwrites and does not restore** the memory it tests; the machine
/// cannot resume and the only sequel is a reset. The safety argument that
/// lets [`run`] run pre-allocator does not apply — the caller must have
/// already taken the machine over (masked interrupts, stopped the watchdog,
/// quiesced the other CPUs).
///
/// The driver calls this once per pattern in [`RamTestPattern::ALL`] and
/// repeats the whole cycle until the machine is reset, so this function tests
/// one pattern over all of RAM and returns; it never loops or resets itself.
///
/// A region the direct map cannot reach is left untested and uncounted rather
/// than faked as tested (fail closed), exactly as in [`run`].
pub fn sweep_pattern<M>(
    map: &BootMemoryMap,
    physmap: &M,
    pattern: RamTestPattern,
    total: u64,
    observer: &mut dyn SweepObserver,
) where
    M: PhysMap + ?Sized,
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
    use crate::bootinfo::MemoryRegion;
    use crate::phys::SimPhysMap;

    extern crate std;
    use core::cell::Cell;
    use std::vec::Vec;

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

    /// A [`SweepObserver`] that records the last progress figures and every
    /// fault, for driving [`sweep_pattern`] on the host.
    #[derive(Default)]
    struct RecordingObserver {
        last_tested: u64,
        last_total: u64,
        faults: Vec<RamFault>,
    }

    impl SweepObserver for RecordingObserver {
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

        let total = takeover_test_bytes(&map);
        assert_eq!(
            total, len as u64,
            "the honest denominator is all usable RAM"
        );
        let mut observer = RecordingObserver::default();
        sweep_pattern(
            &map,
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
        let total = takeover_test_bytes(&map);
        let mut observer = RecordingObserver::default();
        sweep_pattern(
            &map,
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
    fn usable_frame_bytes_counts_only_frame_aligned_usable_span() {
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
        assert_eq!(usable_frame_bytes(&map), 2 * PAGE_SIZE as u64);
    }
}
