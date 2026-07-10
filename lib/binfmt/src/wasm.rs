//! Read-only, fail-closed view of a WebAssembly module's structure.
//!
//! Decodes the section directory and the type/function/code section
//! framing — the function-body boundaries a disassembler walks — per the
//! WebAssembly core specification's binary format. It does not decode
//! instruction payloads (that is `lib/disasm`'s job) and never executes
//! anything.
//!
//! Every LEB128 length is bounds- and length-checked: an encoding longer
//! than five bytes, or one with padding bits set in its final byte (the
//! classic overlong-LEB attack), is a typed [`WasmError`] — never a wrap,
//! never a partial trust of later bytes. The section directory is capped
//! at [`MAX_MODULE_SECTIONS`]; function bodies decode lazily, so a module
//! with a huge code section costs only the bodies actually walked.

use alloc::vec::Vec;

/// Fixed validation cap on the number of sections in one module.
pub const MAX_MODULE_SECTIONS: usize = 1024;

/// Section id of the type section.
pub const SECTION_TYPE: u8 = 1;

/// Section id of the function section.
pub const SECTION_FUNCTION: u8 = 3;

/// Section id of the code section.
pub const SECTION_CODE: u8 = 10;

/// Highest section id defined by the WebAssembly core specification
/// (12, the data-count section).
pub const SECTION_ID_MAX: u8 = 12;

/// Why a wasm input was refused.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WasmError {
    /// The input is shorter than the structure it must contain.
    TooSmall,
    /// The `\0asm` magic is absent.
    BadMagic,
    /// The module's binary-format version is not 1.
    UnsupportedVersion,
    /// A LEB128 length is unterminated, longer than five bytes, or has
    /// padding bits set in its final byte.
    BadLeb,
    /// A declared section or body extent falls outside its container.
    OutOfBounds,
    /// The module declares more than [`MAX_MODULE_SECTIONS`] sections.
    TooManySections,
    /// A section id exceeds [`SECTION_ID_MAX`].
    BadSectionId,
    /// A non-custom section id appears twice.
    DuplicateSection,
    /// A section's payload has bytes left over after its declared
    /// contents (framing and payload length disagree).
    TrailingBytes,
}

impl core::fmt::Display for WasmError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooSmall => f.write_str("input shorter than required structure"),
            Self::BadMagic => f.write_str("missing wasm magic"),
            Self::UnsupportedVersion => f.write_str("unsupported wasm binary version"),
            Self::BadLeb => f.write_str("malformed LEB128 length"),
            Self::OutOfBounds => f.write_str("declared extent falls outside its container"),
            Self::TooManySections => f.write_str("section count exceeds validation cap"),
            Self::BadSectionId => f.write_str("unknown section id"),
            Self::DuplicateSection => f.write_str("duplicate non-custom section"),
            Self::TrailingBytes => f.write_str("payload length and framing disagree"),
        }
    }
}

/// One entry in the module's section directory.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SectionEntry {
    /// Section id (0 custom, 1 type, 3 function, 10 code, …).
    pub id: u8,
    /// File offset of the section's payload.
    pub offset: usize,
    /// Payload size in bytes.
    pub size: usize,
}

/// Decode one LEB128 `u32` at `cursor`, returning the value and the
/// offset just past it.
///
/// Strict per the wasm spec: at most five bytes, and the fifth byte's
/// four padding bits must be clear, so every accepted encoding fits `u32`
/// exactly.
fn leb_u32(bytes: &[u8], cursor: usize) -> Result<(u32, usize), WasmError> {
    let mut value: u32 = 0;
    for i in 0..5 {
        let offset = cursor.checked_add(i).ok_or(WasmError::OutOfBounds)?;
        let byte = *bytes.get(offset).ok_or(WasmError::TooSmall)?;
        if i == 4 && byte & 0xF0 != 0 {
            return Err(WasmError::BadLeb);
        }
        value |= u32::from(byte & 0x7F) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok((value, offset + 1));
        }
    }
    Err(WasmError::BadLeb)
}

/// A validated view of a module's section directory.
///
/// [`WasmView::parse`] walks and bounds-checks the whole directory once;
/// section payloads and function bodies decode lazily on access.
#[derive(Clone, Debug)]
pub struct WasmView<'a> {
    bytes: &'a [u8],
    sections: Vec<SectionEntry>,
}

impl<'a> WasmView<'a> {
    /// Decode and validate `bytes` as a wasm module's section directory.
    ///
    /// # Errors
    ///
    /// A typed [`WasmError`] naming the first violated invariant; the
    /// input is rejected whole.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, WasmError> {
        if bytes.len() < 8 {
            return Err(WasmError::TooSmall);
        }
        if bytes[0..4] != *b"\0asm" {
            return Err(WasmError::BadMagic);
        }
        if bytes[4..8] != 1u32.to_le_bytes() {
            return Err(WasmError::UnsupportedVersion);
        }

        let mut sections = Vec::new();
        let mut seen: u16 = 0; // bitmask over the 12 non-custom ids
        let mut cursor = 8;
        while cursor < bytes.len() {
            if sections.len() == MAX_MODULE_SECTIONS {
                return Err(WasmError::TooManySections);
            }
            let id = bytes[cursor];
            if id > SECTION_ID_MAX {
                return Err(WasmError::BadSectionId);
            }
            if id != 0 {
                let bit = 1u16 << (id - 1);
                if seen & bit != 0 {
                    return Err(WasmError::DuplicateSection);
                }
                seen |= bit;
            }
            let (size, payload) = leb_u32(bytes, cursor + 1)?;
            let size = usize::try_from(size).map_err(|_| WasmError::OutOfBounds)?;
            let end = payload.checked_add(size).ok_or(WasmError::OutOfBounds)?;
            if end > bytes.len() {
                return Err(WasmError::OutOfBounds);
            }
            sections.push(SectionEntry {
                id,
                offset: payload,
                size,
            });
            cursor = end;
        }
        Ok(Self { bytes, sections })
    }

    /// The section directory, in file order.
    #[must_use]
    pub fn sections(&self) -> &[SectionEntry] {
        &self.sections
    }

    /// The payload bytes of a directory entry.
    #[must_use]
    pub fn section_bytes(&self, entry: &SectionEntry) -> &'a [u8] {
        // The directory walk validated every entry's extent, and a
        // caller-forged entry outside the file yields the empty slice
        // rather than a panic.
        self.bytes
            .get(entry.offset..entry.offset.saturating_add(entry.size))
            .unwrap_or(&[])
    }

    /// The first directory entry with `id`, if the module has one.
    #[must_use]
    pub fn section(&self, id: u8) -> Option<&SectionEntry> {
        self.sections.iter().find(|s| s.id == id)
    }

    /// The declared entry count of section `id`, or `None` when the
    /// module has no such section.
    ///
    /// Works for the vector-shaped sections (type, function, and the
    /// other vector sections), whose payload begins with a LEB128 count.
    ///
    /// # Errors
    ///
    /// [`WasmError::BadLeb`] or [`WasmError::TooSmall`] for a malformed
    /// count.
    pub fn entry_count(&self, id: u8) -> Result<Option<u32>, WasmError> {
        let Some(entry) = self.section(id) else {
            return Ok(None);
        };
        let payload = self.section_bytes(entry);
        let (count, _) = leb_u32(payload, 0)?;
        Ok(Some(count))
    }

    /// Walk the code section's function bodies.
    ///
    /// Returns `None` when the module has no code section. Bodies decode
    /// lazily: each iterator step validates one body's size field and
    /// bounds.
    ///
    /// # Errors
    ///
    /// [`WasmError::BadLeb`] or [`WasmError::TooSmall`] for a malformed
    /// body count.
    pub fn code_bodies(&self) -> Result<Option<FunctionBodies<'a>>, WasmError> {
        let Some(entry) = self.section(SECTION_CODE) else {
            return Ok(None);
        };
        let payload = self.section_bytes(entry);
        let (count, after_count) = leb_u32(payload, 0)?;
        Ok(Some(FunctionBodies {
            payload,
            file_base: entry.offset,
            cursor: after_count,
            remaining: count,
            index: 0,
            finished: false,
        }))
    }
}

/// One function body's location: its index and the file extent of its
/// locals-plus-instructions payload.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BodyRange {
    /// Position of the body within the code section (the first body is 0).
    pub index: u32,
    /// File offset of the body's first byte.
    pub offset: usize,
    /// Body size in bytes.
    pub size: usize,
}

/// Lazy iterator over the code section's function bodies.
///
/// Yields one [`BodyRange`] per body; a framing violation yields one
/// `Err` and then the iterator ends (fail closed — later bytes are never
/// trusted past a malformed length).
#[derive(Clone, Debug)]
pub struct FunctionBodies<'a> {
    payload: &'a [u8],
    file_base: usize,
    cursor: usize,
    remaining: u32,
    index: u32,
    finished: bool,
}

impl FunctionBodies<'_> {
    /// Number of bodies the section declares in total.
    #[must_use]
    pub fn declared(&self) -> u32 {
        self.remaining + self.index
    }
}

impl Iterator for FunctionBodies<'_> {
    type Item = Result<BodyRange, WasmError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        if self.remaining == 0 {
            self.finished = true;
            // The declared bodies must fill the payload exactly.
            if self.cursor != self.payload.len() {
                return Some(Err(WasmError::TrailingBytes));
            }
            return None;
        }
        let step = (|| {
            let (size, body_start) = leb_u32(self.payload, self.cursor)?;
            let size = usize::try_from(size).map_err(|_| WasmError::OutOfBounds)?;
            let end = body_start.checked_add(size).ok_or(WasmError::OutOfBounds)?;
            if end > self.payload.len() {
                return Err(WasmError::OutOfBounds);
            }
            Ok((body_start, size, end))
        })();
        match step {
            Ok((body_start, size, end)) => {
                let range = BodyRange {
                    index: self.index,
                    offset: self.file_base + body_start,
                    size,
                };
                self.cursor = end;
                self.remaining -= 1;
                self.index += 1;
                Some(Ok(range))
            }
            Err(e) => {
                self.finished = true;
                Some(Err(e))
            }
        }
    }
}

#[cfg(test)]
#[path = "wasm_tests.rs"]
mod tests;
