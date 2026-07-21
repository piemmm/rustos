//! Multiboot2 information-structure builder.
//!
//! The first-party TAIRiX loader assembles the information structure the
//! kernel [`crate::BootInfo`] parser reads, into a caller-provided buffer,
//! before it enters the kernel. The builder is allocation-free (it writes
//! into the buffer the loader reserved) and fail-closed: every append that
//! would overflow the buffer, overflow a length field, or carry a
//! malformed command line is a typed [`BuildError`], never a partial or
//! silently-truncated structure.
//!
//! Usage is append-then-[`finish`](InfoBuilder::finish): the header is
//! reserved up front, each tag is appended 8-byte aligned, and `finish`
//! writes the terminating end tag and back-fills `total_size`. The
//! resulting bytes must be placed on an 8-byte boundary for the parser to
//! accept them (the Multiboot2 spec requires the structure to be so
//! aligned); the loader shell owns that placement.

use crate::parse::{FramebufferInfo, Mb2MemoryEntry};
use crate::{
    align8, put_u32, put_u64, HEADER_BYTES, TAG_BASIC_MEMORY, TAG_CMDLINE, TAG_END,
    TAG_FRAMEBUFFER, TAG_MMAP, TAG_RSDP_V1, TAG_RSDP_V2,
};

/// A build failed to fit or would have produced a malformed structure. All
/// variants are fail-closed refusals (the buffer is left untrusted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildError {
    /// The caller-provided buffer cannot hold this tag plus the mandatory
    /// terminating end tag.
    BufferTooSmall,
    /// A tag `size` or a running offset would not fit a `u32` / `usize`.
    Overflow,
    /// A command-line string contained an interior NUL byte, which the
    /// null-terminated encoding cannot represent without truncation.
    InvalidString,
}

/// Builder for a Multiboot2 information structure, writing into a
/// caller-provided byte buffer.
///
/// Construct with [`InfoBuilder::new`], append tags, then call
/// [`InfoBuilder::finish`] to terminate the stream and obtain the written
/// bytes. See the module docs for the contract.
#[derive(Debug)]
pub struct InfoBuilder<'a> {
    buf: &'a mut [u8],
    /// Bytes written so far, always 8-byte aligned. Starts at
    /// [`HEADER_BYTES`] (the reserved information header).
    len: usize,
}

impl<'a> InfoBuilder<'a> {
    /// Start building into `buf`.
    ///
    /// # Errors
    ///
    /// [`BuildError::BufferTooSmall`] if `buf` cannot even hold the 8-byte
    /// information header plus the 8-byte terminating end tag.
    pub fn new(buf: &'a mut [u8]) -> Result<Self, BuildError> {
        if buf.len() < HEADER_BYTES + HEADER_BYTES {
            return Err(BuildError::BufferTooSmall);
        }
        Ok(Self {
            buf,
            len: HEADER_BYTES,
        })
    }

    /// Reserve space for one tag: write its 8-byte header, zero the
    /// trailing alignment padding, advance the cursor, and return the
    /// mutable payload slice for the caller to fill.
    ///
    /// The reservation always leaves room for the mandatory 8-byte end tag
    /// so [`InfoBuilder::finish`] can never fail.
    fn append(&mut self, tag_type: u32, payload_len: usize) -> Result<&mut [u8], BuildError> {
        let start = self.len;
        let size = payload_len
            .checked_add(HEADER_BYTES)
            .ok_or(BuildError::Overflow)?;
        let advance = align8(size).ok_or(BuildError::Overflow)?;
        let end = start.checked_add(advance).ok_or(BuildError::Overflow)?;
        // Leave room for the terminating end tag (8 bytes).
        let needed = end.checked_add(HEADER_BYTES).ok_or(BuildError::Overflow)?;
        if needed > self.buf.len() {
            return Err(BuildError::BufferTooSmall);
        }
        // The whole structure's `total_size` is a `u32` in the header, so
        // the running length must stay within `u32` range.
        if u32::try_from(needed).is_err() {
            return Err(BuildError::Overflow);
        }
        let size_u32 = u32::try_from(size).map_err(|_| BuildError::Overflow)?;
        put_u32(self.buf, start, tag_type);
        put_u32(self.buf, start + 4, size_u32);
        // Zero the alignment padding between the payload end and `advance`.
        for b in &mut self.buf[start + size..end] {
            *b = 0;
        }
        self.len = end;
        Ok(&mut self.buf[start + HEADER_BYTES..start + size])
    }

    /// Append the basic-memory tag (type 4): lower/upper memory in KiB.
    ///
    /// # Errors
    ///
    /// See [`BuildError`].
    pub fn basic_memory(&mut self, lower_kib: u32, upper_kib: u32) -> Result<(), BuildError> {
        let p = self.append(TAG_BASIC_MEMORY, 8)?;
        p[0..4].copy_from_slice(&lower_kib.to_le_bytes());
        p[4..8].copy_from_slice(&upper_kib.to_le_bytes());
        Ok(())
    }

    /// Append the boot-command-line tag (type 1): a null-terminated string.
    ///
    /// # Errors
    ///
    /// [`BuildError::InvalidString`] if `cmdline` contains an interior NUL
    /// (which the null-terminated encoding cannot represent); otherwise see
    /// [`BuildError`].
    pub fn command_line(&mut self, cmdline: &str) -> Result<(), BuildError> {
        let bytes = cmdline.as_bytes();
        if bytes.contains(&0) {
            return Err(BuildError::InvalidString);
        }
        let payload_len = bytes.len().checked_add(1).ok_or(BuildError::Overflow)?;
        let p = self.append(TAG_CMDLINE, payload_len)?;
        p[..bytes.len()].copy_from_slice(bytes);
        p[bytes.len()] = 0;
        Ok(())
    }

    /// Append the ACPI RSDP tag: type 15 (`v2 == true`, XSDT-capable) or
    /// type 14 (`v2 == false`), carrying the raw RSDP descriptor bytes.
    ///
    /// # Errors
    ///
    /// See [`BuildError`].
    pub fn rsdp(&mut self, v2: bool, descriptor: &[u8]) -> Result<(), BuildError> {
        let tag = if v2 { TAG_RSDP_V2 } else { TAG_RSDP_V1 };
        let p = self.append(tag, descriptor.len())?;
        p.copy_from_slice(descriptor);
        Ok(())
    }

    /// Append the memory-map tag (type 6) built from `entries`. Each entry
    /// is emitted with the fixed 24-byte layout (base, length, canonical
    /// raw type, reserved) and `entry_version == 0`.
    ///
    /// # Errors
    ///
    /// See [`BuildError`].
    pub fn memory_map(&mut self, entries: &[Mb2MemoryEntry]) -> Result<(), BuildError> {
        const ENTRY_SIZE: usize = 24;
        let body_len = entries
            .len()
            .checked_mul(ENTRY_SIZE)
            .ok_or(BuildError::Overflow)?;
        // 4-byte entry_size + 4-byte entry_version header before entries.
        let payload_len = body_len.checked_add(8).ok_or(BuildError::Overflow)?;
        let p = self.append(TAG_MMAP, payload_len)?;
        put_u32(
            p,
            0,
            u32::try_from(ENTRY_SIZE).map_err(|_| BuildError::Overflow)?,
        );
        put_u32(p, 4, 0); // entry_version
        for (i, e) in entries.iter().enumerate() {
            let off = 8 + i * ENTRY_SIZE;
            put_u64(p, off, e.base);
            put_u64(p, off + 8, e.length);
            put_u32(p, off + 16, e.kind.to_raw());
            put_u32(p, off + 20, 0); // reserved
        }
        Ok(())
    }

    /// Append the framebuffer tag (type 8). Direct-RGB colour info is
    /// always emitted (the six field-position / mask-size bytes), so the
    /// tag round-trips through the parser regardless of `fb.fb_type`.
    ///
    /// # Errors
    ///
    /// See [`BuildError`].
    pub fn framebuffer(&mut self, fb: &FramebufferInfo) -> Result<(), BuildError> {
        let payload_len = FramebufferInfo::COMMON_LEN + FramebufferInfo::RGB_COLOR_LEN;
        let p = self.append(TAG_FRAMEBUFFER, payload_len)?;
        put_u64(p, 0, fb.addr);
        put_u32(p, 8, fb.pitch);
        put_u32(p, 12, fb.width);
        put_u32(p, 16, fb.height);
        p[20] = fb.bpp;
        p[21] = fb.fb_type;
        // p[22..24] reserved (already zeroed by `append`'s padding? no —
        // it is within `size`, so zero it explicitly).
        p[22] = 0;
        p[23] = 0;
        let c = FramebufferInfo::COMMON_LEN;
        p[c] = fb.red_field_position;
        p[c + 1] = fb.red_mask_size;
        p[c + 2] = fb.green_field_position;
        p[c + 3] = fb.green_mask_size;
        p[c + 4] = fb.blue_field_position;
        p[c + 5] = fb.blue_mask_size;
        Ok(())
    }

    /// Terminate the stream: write the end tag (type 0, size 8), back-fill
    /// the information header's `total_size`, and return the written bytes.
    ///
    /// Each appended tag reserves room for the end tag as it goes, so this
    /// never fails.
    #[must_use]
    pub fn finish(self) -> &'a [u8] {
        let InfoBuilder { buf, len } = self;
        let total = len + HEADER_BYTES;
        // `append` guarantees `len + HEADER_BYTES` fits a `u32`, so both
        // casts below are lossless; `HEADER_BYTES` is 8.
        let end_size = u32::try_from(HEADER_BYTES).unwrap_or(u32::MAX);
        let total_u32 = u32::try_from(total).unwrap_or(u32::MAX);
        // End tag: type 0, size 8.
        put_u32(buf, len, TAG_END);
        put_u32(buf, len + 4, end_size);
        // Information header: total_size, reserved (0).
        put_u32(buf, 0, total_u32);
        put_u32(buf, 4, 0);
        &buf[..total]
    }
}
