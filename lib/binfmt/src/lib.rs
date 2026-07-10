//! RustOS read-only executable-container decoder (`lib/binfmt`).
//!
//! Several RustOS components need to *look inside* an executable file
//! without loading it: the file manager's disassembly viewer first, and an
//! `objdump`/`readelf`-class command app next. That decoding is identical
//! wherever it happens, so it lives here once. The crate produces typed,
//! borrowed views of three containers:
//!
//! - **`rxe`** ([`rxe::RxeView`], [`rxe::ManifestSummary`]) — the RustOS
//!   load image and the signed manifest, decoded *through* the `lib/abi`
//!   wire types (`LoadImage::parse_for_inspection`,
//!   `ManifestHeader::from_bytes`, `decode_capability_ids`), so the
//!   inspection view and the kernel load path share one definition and
//!   cannot drift.
//! - **ELF64** ([`elf::ElfView`]) — little-endian `EM_X86_64` /
//!   `EM_AARCH64` / `EM_RISCV` files: the file header, program headers,
//!   section headers, section-name strings, and symbol tables — enough to
//!   name and bound the code regions a disassembler walks.
//! - **wasm** ([`wasm::WasmView`]) — module structure: the section
//!   directory and the type/function/code section framing (function-body
//!   boundaries), with strictly validated LEB128 lengths.
//!
//! # Untrusted input, read-only, fail closed
//!
//! Every input is untrusted (any file a user points a viewer at). The
//! decoders never execute, map, or load anything; every offset and length
//! is bounds-checked against the input slice before use, every table
//! carries a fixed validation cap, and a malformed input is a typed error
//! naming what failed — never a panic and never a partial trust of later
//! bytes. Consumers run these decoders inside the minimum-capability
//! parser sandbox; the functions here are pure, total slice decoders
//! precisely so they link into that sandbox process unchanged.
//!
//! # Example
//!
//! ```
//! use rustos_binfmt::{detect, Format};
//!
//! assert_eq!(detect(b"\x7fELF\x02\x01\x01\x00"), Some(Format::Elf64));
//! assert_eq!(detect(b"\0asm\x01\x00\x00\x00"), Some(Format::Wasm));
//! assert_eq!(detect(b"plain text"), None);
//! ```

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod elf;
pub mod rxe;
pub mod wasm;

/// An executable-container format this crate can decode.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Format {
    /// A RustOS `rxe` load image (`RXEL` magic).
    Rxe,
    /// A 64-bit little-endian ELF file (`\x7fELF`, `ELFCLASS64`,
    /// `ELFDATA2LSB`).
    Elf64,
    /// A WebAssembly module (`\0asm`).
    Wasm,
}

/// Recognise the container format of `bytes` from its magic prefix.
///
/// Detection is deliberately shallow — it answers "which decoder should
/// look at this file?", not "is this file valid?". The matching decoder
/// still validates everything and fails closed on a malformed body. For
/// ELF, only the 64-bit little-endian class this crate decodes is
/// recognised; a 32-bit or big-endian ELF returns `None` so a caller falls
/// back honestly (to a hex view) instead of failing half-way in.
#[must_use]
pub fn detect(bytes: &[u8]) -> Option<Format> {
    if bytes.len() >= 4 && bytes[0..4] == rustos_abi::LOAD_MAGIC.to_le_bytes() {
        return Some(Format::Rxe);
    }
    if bytes.len() >= 6
        && bytes[0..4] == *b"\x7fELF"
        && bytes[4] == elf::ELF_CLASS_64
        && bytes[5] == elf::ELF_DATA_LSB
    {
        return Some(Format::Elf64);
    }
    if bytes.len() >= 4 && bytes[0..4] == *b"\0asm" {
        return Some(Format::Wasm);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{detect, Format};

    #[test]
    fn detects_each_magic() {
        assert_eq!(detect(b"RXEL\x01\x00\x00\x00"), Some(Format::Rxe));
        assert_eq!(detect(b"\x7fELF\x02\x01\x01\x00"), Some(Format::Elf64));
        assert_eq!(detect(b"\0asm\x01\x00\x00\x00"), Some(Format::Wasm));
    }

    #[test]
    fn rejects_non_executables_and_short_prefixes() {
        assert_eq!(detect(b""), None);
        assert_eq!(detect(b"RXE"), None);
        assert_eq!(detect(b"\x7fEL"), None);
        assert_eq!(detect(b"plain text file"), None);
        assert_eq!(detect(&[0u8; 16]), None);
    }

    #[test]
    fn rejects_elf_classes_this_crate_does_not_decode() {
        // 32-bit ELF.
        assert_eq!(detect(b"\x7fELF\x01\x01\x01\x00"), None);
        // Big-endian ELF64.
        assert_eq!(detect(b"\x7fELF\x02\x02\x01\x00"), None);
        // Magic alone, class byte missing.
        assert_eq!(detect(b"\x7fELF"), None);
    }
}
