//! Firmware-neutral loader core for the TAIRiX boot chain.
//!
//! A TAIRiX loader (the UEFI application, the legacy-BIOS stub) must place
//! the kernel ELF image into physical memory before it can hand control
//! over. The *decision* of what to place — which segments, where, how big,
//! and with which permissions — is identical on every firmware; only the
//! *act* of placing bytes (a UEFI `AllocatePages` + `CopyMem`, a BIOS
//! real-mode copy) differs. This crate is that shared decision, computed
//! once and reused by every firmware shell, so two loaders can never
//! disagree on how the kernel is laid out.
//!
//! [`plan_kernel_load`] decodes the image through the shared
//! [`tairix_binfmt::elf`] view and returns a validated [`LoadPlan`]: the
//! `PT_LOAD` segments (each a file source range, a physical destination, a
//! memory size whose trailing bytes past the file content are zero-filled,
//! and the read/write/execute permission flags), the entry point, and the
//! physical span the firmware must have free.
//!
//! The core touches no hardware and allocates nothing: the segment list is
//! a fixed, bounded array ([`MAX_LOAD_SEGMENTS`]) — a security limit on an
//! untrusted image, not a growable capacity — so the crate builds in a
//! firmware environment with no heap. Every field is bounds- and
//! shape-checked before it is trusted; a malformed or hostile image is a
//! typed [`LoadError`], never a panic and never a partial trust of later
//! bytes.
//!
//! What the core deliberately does **not** do: it never marks a segment
//! both writable and executable (a write-xor-execute image is rejected),
//! it never lets two segments' destinations overlap, and it never trusts a
//! file range that runs past the image. Those refusals are the loader's
//! first line of defence, made before any byte is copied to memory.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use tairix_binfmt::elf::{ElfError, ElfView, Machine};

/// ELF `e_type` value for an executable object file (`ET_EXEC`).
///
/// The TAIRiX kernel is linked as a fixed-address executable; the loader
/// refuses anything else so it never tries to place a relocatable or
/// shared object it cannot honour.
pub const ET_EXEC: u16 = 2;

/// ELF program-header `p_type` value for a loadable segment (`PT_LOAD`).
pub const PT_LOAD: u32 = 1;

/// `p_flags` bit for an executable segment (`PF_X`).
pub const PF_X: u32 = 0x1;

/// `p_flags` bit for a writable segment (`PF_W`).
pub const PF_W: u32 = 0x2;

/// `p_flags` bit for a readable segment (`PF_R`).
pub const PF_R: u32 = 0x4;

/// Maximum number of loadable segments the loader will place.
///
/// This is a fixed security bound on an untrusted image, not a growable
/// capacity: a well-formed TAIRiX kernel has a handful of `PT_LOAD`
/// segments, and an image that claims more is rejected rather than
/// consuming unbounded loader state. Raising it is a deliberate security
/// decision, never a convenience.
pub const MAX_LOAD_SEGMENTS: usize = 16;

/// The read/write/execute permissions a segment is mapped with, decoded
/// from the ELF `p_flags` field.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SegmentFlags {
    /// The segment's bytes are readable.
    pub readable: bool,
    /// The segment's bytes are writable.
    pub writable: bool,
    /// The segment's bytes are executable.
    pub executable: bool,
}

impl SegmentFlags {
    /// Decode the permission bits from an ELF `p_flags` word.
    #[must_use]
    pub fn from_p_flags(p_flags: u32) -> Self {
        Self {
            readable: p_flags & PF_R != 0,
            writable: p_flags & PF_W != 0,
            executable: p_flags & PF_X != 0,
        }
    }

    /// Whether the segment is both writable and executable.
    ///
    /// A write-xor-execute boot image never leaves a segment in this
    /// state; the loader rejects one that does rather than placing an
    /// attacker-writable code page.
    #[must_use]
    pub fn is_write_execute(self) -> bool {
        self.writable && self.executable
    }
}

/// One loadable segment the firmware must place in physical memory.
///
/// The firmware copies `file_size` bytes from `file_offset` in the image
/// to physical address `phys_dest`, then zero-fills the `mem_size -
/// file_size` bytes that follow (the segment's BSS tail). `flags` is the
/// permission set the segment is ultimately mapped with.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct LoadSegment {
    /// Byte offset of the segment's file content within the kernel image.
    pub file_offset: u64,
    /// Number of bytes to copy from the image (`p_filesz`).
    pub file_size: u64,
    /// Physical address the segment is placed at (`p_paddr`).
    pub phys_dest: u64,
    /// Total in-memory size of the segment (`p_memsz`); the tail past
    /// `file_size` is zero-filled.
    pub mem_size: u64,
    /// Read/write/execute permissions decoded from `p_flags`.
    pub flags: SegmentFlags,
}

impl LoadSegment {
    /// The exclusive physical end address of this segment
    /// (`phys_dest + mem_size`).
    ///
    /// Returns `None` if the sum would overflow `u64`; the planner treats
    /// that as a malformed segment and fails closed.
    #[must_use]
    pub fn phys_end(&self) -> Option<u64> {
        self.phys_dest.checked_add(self.mem_size)
    }
}

/// A validated plan describing how to load a kernel image into memory.
///
/// Produced by [`plan_kernel_load`]. The segment list is a fixed-capacity
/// array; use [`LoadPlan::segments`] to iterate only the populated entries.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct LoadPlan {
    segments: [LoadSegment; MAX_LOAD_SEGMENTS],
    count: usize,
    entry: u64,
}

impl LoadPlan {
    /// The kernel entry point (`e_entry`) control transfers to once every
    /// segment has been placed.
    #[must_use]
    pub fn entry(&self) -> u64 {
        self.entry
    }

    /// The populated load segments, in program-header order.
    #[must_use]
    pub fn segments(&self) -> &[LoadSegment] {
        &self.segments[..self.count]
    }

    /// The lowest physical destination and the exclusive highest physical
    /// end across every segment — the contiguous window (which may contain
    /// gaps between segments) the firmware must have available.
    ///
    /// Returns `None` only for an empty plan, which [`plan_kernel_load`]
    /// never produces (it rejects an image with no loadable segment).
    #[must_use]
    pub fn phys_span(&self) -> Option<(u64, u64)> {
        let populated = self.segments();
        let first = populated.first()?;
        let mut lo = first.phys_dest;
        let mut hi = first.phys_end()?;
        for seg in &populated[1..] {
            if seg.phys_dest < lo {
                lo = seg.phys_dest;
            }
            let end = seg.phys_end()?;
            if end > hi {
                hi = end;
            }
        }
        Some((lo, hi))
    }
}

/// Everything that can make an image unloadable. Every variant is a
/// refusal: the loader places nothing when planning fails.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum LoadError {
    /// The image is not a well-formed ELF64 file (the shared decoder said
    /// so), carrying the underlying decode failure.
    Elf(ElfError),
    /// The image is not an `ET_EXEC` executable (it is relocatable, a
    /// shared object, or a core file).
    NotExecutable,
    /// The image targets a different instruction set than the loader
    /// expected.
    WrongMachine(Machine),
    /// The image declares no loadable (`PT_LOAD`) segment.
    NoLoadableSegments,
    /// The image declares more loadable segments than the loader will
    /// place ([`MAX_LOAD_SEGMENTS`]).
    TooManySegments,
    /// A segment's file content (`p_filesz`) does not fit within the
    /// image, or an offset/size computation overflowed.
    FileRangeOutOfBounds,
    /// A segment's file size exceeds its memory size, so its bytes would
    /// not fit where it is loaded.
    FileLargerThanMemory,
    /// A segment has a zero memory size and would place nothing.
    EmptySegment,
    /// A segment's physical placement or size overflows the address space.
    PhysRangeOverflow,
    /// A segment's alignment constraint is not a power of two, or its
    /// physical destination is not congruent with its file offset modulo
    /// that alignment.
    MisalignedSegment,
    /// Two segments' physical destination ranges overlap.
    SegmentOverlap,
    /// A segment is both writable and executable.
    WritableAndExecutable,
}

impl From<ElfError> for LoadError {
    fn from(err: ElfError) -> Self {
        LoadError::Elf(err)
    }
}

/// Compute the [`LoadPlan`] for a kernel ELF image, or explain why the
/// image cannot be loaded.
///
/// `image` is the raw kernel ELF bytes; `expected_machine` is the
/// instruction set the calling firmware runs (so the same core serves
/// every architecture without naming one). The returned plan lists the
/// `PT_LOAD` segments to place and the entry point to transfer to.
///
/// Fails closed: the image is validated in full before any segment is
/// accepted, and any malformed, oversized, overlapping, misaligned, or
/// write-executable segment rejects the whole image.
pub fn plan_kernel_load(image: &[u8], expected_machine: Machine) -> Result<LoadPlan, LoadError> {
    let view = ElfView::parse(image)?;
    let header = view.header();

    if header.e_type != ET_EXEC {
        return Err(LoadError::NotExecutable);
    }
    if header.machine != expected_machine {
        return Err(LoadError::WrongMachine(header.machine));
    }

    let image_len = image.len() as u64;
    let empty = LoadSegment {
        file_offset: 0,
        file_size: 0,
        phys_dest: 0,
        mem_size: 0,
        flags: SegmentFlags {
            readable: false,
            writable: false,
            executable: false,
        },
    };
    let mut segments = [empty; MAX_LOAD_SEGMENTS];
    let mut count = 0usize;

    for index in 0..header.phnum {
        let ph = view.program_header(index)?;
        if ph.p_type != PT_LOAD {
            continue;
        }
        if ph.mem_size == 0 {
            return Err(LoadError::EmptySegment);
        }
        if ph.file_size > ph.mem_size {
            return Err(LoadError::FileLargerThanMemory);
        }

        // The file content must lie wholly within the image.
        let file_end = ph
            .offset
            .checked_add(ph.file_size)
            .ok_or(LoadError::FileRangeOutOfBounds)?;
        if file_end > image_len {
            return Err(LoadError::FileRangeOutOfBounds);
        }

        // The physical placement must not wrap the address space.
        let phys_end = ph
            .paddr
            .checked_add(ph.mem_size)
            .ok_or(LoadError::PhysRangeOverflow)?;

        // Alignment must be a power of two (0 and 1 mean "no constraint"),
        // and the destination must share the file offset's residue so the
        // page mapping the firmware builds can honour it.
        if ph.align > 1 {
            if !ph.align.is_power_of_two() {
                return Err(LoadError::MisalignedSegment);
            }
            let mask = ph.align - 1;
            if (ph.paddr & mask) != (ph.offset & mask) {
                return Err(LoadError::MisalignedSegment);
            }
        }

        let flags = SegmentFlags::from_p_flags(ph.flags);
        if flags.is_write_execute() {
            return Err(LoadError::WritableAndExecutable);
        }

        // Reject a destination that overlaps any already-accepted segment.
        for existing in &segments[..count] {
            let existing_end = existing.phys_end().ok_or(LoadError::PhysRangeOverflow)?;
            if ph.paddr < existing_end && existing.phys_dest < phys_end {
                return Err(LoadError::SegmentOverlap);
            }
        }

        if count == MAX_LOAD_SEGMENTS {
            return Err(LoadError::TooManySegments);
        }
        segments[count] = LoadSegment {
            file_offset: ph.offset,
            file_size: ph.file_size,
            phys_dest: ph.paddr,
            mem_size: ph.mem_size,
            flags,
        };
        count += 1;
    }

    if count == 0 {
        return Err(LoadError::NoLoadableSegments);
    }

    Ok(LoadPlan {
        segments,
        count,
        entry: header.entry,
    })
}

#[cfg(test)]
mod tests;
