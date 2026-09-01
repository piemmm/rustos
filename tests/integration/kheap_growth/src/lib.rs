//! `plans/FIX-KHEAP.md` growth-path QEMU exercise: drive the kernel heap
//! past its bootstrap region against the port's **real** kernel remap window
//! and dereference every page of the assembled run.
//!
//! ## Why it lives here rather than in each vertical
//!
//! The growth path assembles a region out of many physical chunks and maps
//! them into one virtually-contiguous run of the port's remap window. A host
//! test cannot dereference a window address — there is no hardware to map it
//! — so `kernel/core`'s tests drive the growth source's *contract* over a
//! non-allocating page-table double and stop there. What only a guest can
//! prove is that the assembled run is genuinely readable and writable under
//! the live translation root, on each port's own window placement.
//!
//! The exercise itself is architecture-neutral: it reaches the hardware only
//! through the heap. So it is one definition here, driven by each port's
//! boot-completed vertical, rather than a copy per port.
//!
//! ## What [`verify`] proves
//!
//! 1. A request larger than the heap's free remainder is satisfied. Nothing
//!    already mapped can serve it, so it must come from a freshly grown
//!    region — the smallest request for which that is guaranteed.
//! 2. The heap's capacity rose, so a region really was assembled rather than
//!    the request being squeezed into existing space.
//! 3. Every page of the run carries back the marker written to it. A
//!    mistranslated leaf, a chunk mapped twice, or a run that overlaps
//!    another allocation all show up as a mismatch.
//! 4. Freeing the block drains the region, which drives `shrink` — the
//!    teardown that unmaps the run, invalidates it, and hands the frames and
//!    the window address space back.
//! 5. A second round succeeds identically. A teardown that stranded window
//!    address space, leaked its bookkeeping, or left a stale leaf behind
//!    cannot serve the same request twice.
//!
//! The post-free capacity is *reported* rather than asserted: another CPU may
//! legitimately allocate into the drained region's remainder within the
//! window, so a byte-exact verdict on it would be a load-dependent one.

#![no_std]
#![deny(missing_docs)]

use core::alloc::{GlobalAlloc, Layout};

use tairix_abi::PAGE_SIZE;
use tairix_kalloc::FreeListAllocator;
use tairix_log::{log, Event, EventId, Field, FieldValue, Level, Sink};

/// Rounds the exercise runs. Two: the second is what proves the first
/// round's teardown left the window reusable.
const ROUNDS: usize = 2;

/// Marker base written once per page. Chosen so a zero page, an all-ones
/// page, and a page holding another allocation's data are all
/// distinguishable from a correctly mapped one.
const MARKER: u64 = 0xA5A5_5A5A_0000_0000;

/// Width of the per-page marker.
const MARKER_BYTES: usize = size_of::<u64>();

const _: () = assert!(PAGE_SIZE >= MARKER_BYTES);

/// Event id for the measured growth record.
pub const KHEAP_GROWTH_MEASURED: EventId = EventId(4380);

/// Event id for the growth-path fault record.
pub const KHEAP_GROWTH_FAULT: EventId = EventId(4381);

/// What the exercise measured, for the QEMU transcript.
///
/// `bootstrap_bytes` and `used_bytes` are the heap's capacity and live bytes
/// as boot left them, so the transcript also carries the boot-time heap
/// watermark a future change to the bootstrap arena's size would need.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Growth {
    /// Heap capacity before the first forced grow.
    pub bootstrap_bytes: usize,
    /// Heap bytes live before the first forced grow.
    pub used_bytes: usize,
    /// Bytes the first forced request asked for.
    pub request_bytes: usize,
    /// Heap capacity at the peak of the first round.
    pub grown_bytes: usize,
    /// Heap capacity after both rounds freed their blocks.
    pub settled_bytes: usize,
}

/// Why the growth path failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fault {
    /// The request the bootstrap region cannot serve was refused, so no
    /// region was grown (or the frame pool could not supply one).
    Refused,
    /// The request was served without the capacity rising, so it did not
    /// come from a grown region and the exercise proves nothing.
    NoGrowth,
    /// A page of the assembled run did not read back its marker.
    Corrupt,
    /// A layout the heap must accept could not be constructed.
    Layout,
}

impl Fault {
    /// Stable label for the transcript.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Refused => "refused",
            Self::NoGrowth => "no_growth",
            Self::Corrupt => "corrupt",
            Self::Layout => "layout",
        }
    }
}

/// Drive the growth path and report what it measured.
///
/// Emits one record on `sink` either way, so the QEMU transcript carries the
/// figures whether the exercise passed or failed; the caller decides how to
/// finish the run.
pub fn verify(heap: &FreeListAllocator, sink: &(dyn Sink + Sync)) -> Result<Growth, Fault> {
    let bootstrap_bytes = heap.capacity();
    let used_bytes = heap.used();

    let (request_bytes, grown_bytes) =
        round(heap).inspect_err(|fault| report_fault(sink, *fault))?;
    for _ in 1..ROUNDS {
        round(heap).inspect_err(|fault| report_fault(sink, *fault))?;
    }

    let measured = Growth {
        bootstrap_bytes,
        used_bytes,
        request_bytes,
        grown_bytes,
        settled_bytes: heap.capacity(),
    };
    report(sink, &measured);
    Ok(measured)
}

/// One grow → dereference → free round, returning `(request, peak capacity)`.
fn round(heap: &FreeListAllocator) -> Result<(usize, usize), Fault> {
    let before = heap.capacity();
    // No single allocation can exceed the free remainder of what is already
    // mapped, so one page past it is the smallest request that must be served
    // out of a freshly grown region.
    let request = heap
        .remaining()
        .checked_add(PAGE_SIZE)
        .ok_or(Fault::Layout)?;
    let layout =
        Layout::from_size_align(request, 2 * size_of::<usize>()).map_err(|_| Fault::Layout)?;

    // SAFETY: the layout has a non-zero size and a power-of-two alignment.
    let block = unsafe { heap.alloc(layout) };
    if block.is_null() {
        return Err(Fault::Refused);
    }
    let peak = heap.capacity();
    let mapped = peak > before;
    let intact = mapped && scribble_and_verify(block, request);
    // SAFETY: `block` came from this heap with this exact layout, and nothing
    // else holds a reference into it.
    unsafe { heap.dealloc(block, layout) };

    if !mapped {
        return Err(Fault::NoGrowth);
    }
    if !intact {
        return Err(Fault::Corrupt);
    }
    Ok((request, peak))
}

/// Write a per-page marker across `[block, block + len)`, then read every
/// one back. One marker per page is what distinguishes a per-leaf mapping
/// error; writing whole pages would only re-prove the same leaves.
///
/// The accesses are volatile so the read-back cannot be forwarded from the
/// write, and the marker is moved as its bytes so no alignment beyond the
/// block's own is assumed.
fn scribble_and_verify(block: *mut u8, len: usize) -> bool {
    let pages = len / PAGE_SIZE;
    for page in 0..pages {
        // SAFETY: `page * PAGE_SIZE + MARKER_BYTES <= len` because
        // `PAGE_SIZE >= MARKER_BYTES`, so the write lands inside the block the
        // heap just handed out.
        unsafe {
            block
                .add(page * PAGE_SIZE)
                .cast::<[u8; MARKER_BYTES]>()
                .write_volatile(marker_for(page));
        }
    }
    for page in 0..pages {
        // SAFETY: as for the write above.
        let seen = unsafe {
            block
                .add(page * PAGE_SIZE)
                .cast::<[u8; MARKER_BYTES]>()
                .read_volatile()
        };
        if seen != marker_for(page) {
            return false;
        }
    }
    true
}

/// The marker page `page` must carry.
fn marker_for(page: usize) -> [u8; MARKER_BYTES] {
    (MARKER ^ page as u64).to_ne_bytes()
}

fn report(sink: &(dyn Sink + Sync), measured: &Growth) {
    log(
        sink,
        &Event {
            level: Level::Info,
            id: KHEAP_GROWTH_MEASURED,
            message: "kernel heap grew past its bootstrap region and the run dereferenced",
            fields: &[
                Field {
                    key: "bootstrap_bytes",
                    value: FieldValue::UnsignedInt(measured.bootstrap_bytes as u64),
                },
                Field {
                    key: "used_bytes",
                    value: FieldValue::UnsignedInt(measured.used_bytes as u64),
                },
                Field {
                    key: "request_bytes",
                    value: FieldValue::UnsignedInt(measured.request_bytes as u64),
                },
                Field {
                    key: "grown_bytes",
                    value: FieldValue::UnsignedInt(measured.grown_bytes as u64),
                },
                Field {
                    key: "settled_bytes",
                    value: FieldValue::UnsignedInt(measured.settled_bytes as u64),
                },
            ],
        },
    );
}

fn report_fault(sink: &(dyn Sink + Sync), fault: Fault) {
    log(
        sink,
        &Event {
            level: Level::Error,
            id: KHEAP_GROWTH_FAULT,
            message: "kernel heap growth path failed",
            fields: &[Field {
                key: "fault",
                value: FieldValue::Str(fault.label()),
            }],
        },
    );
}
