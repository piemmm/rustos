# `tairix-binfmt` — executable-container decoder

`tairix_binfmt` (`lib/binfmt`) is TAIRiX's one definition of how an executable
*file* is read without being loaded: a typed, borrowed, fail-closed view of the
`rxe` load image and manifest, of ELF64, and of wasm module structure. The file
manager's disassembly viewer consumes it first; an `objdump`/`readelf`-class
command app consumes the same crate the moment it lands. Decoding an executable
is reading bytes, not loading code — the crate needs and grants no authority.

Stability tier: **experimental** (the surface grows as callers need it).

## Formats

- **`rxe`** — `rxe::RxeView` decodes the load image *through* the `lib/abi`
  wire types: `LoadImage::parse_for_inspection` runs every structural
  load-time invariant (W^X, page alignment, sorted non-overlapping segments,
  entry inside an executable segment, PIE) and the view reports the header,
  segment table, needed libraries, and CFI tag. The tag is **reported, never
  compared** — an inspection tool has no kernel interface hash, and admitting
  a binary to execution stays the kernel loader's job (`LoadImage::parse`).
  `rxe::ManifestSummary` decodes a signed manifest's header and capability
  request through `ManifestHeader::from_bytes` + `decode_capability_ids`; it
  summarises what the manifest *says* and performs no signature verification
  (that needs the install's authority key and belongs to the load gate).
  Because the decoders *are* the ABI types, the inspection view and the load
  path can never drift.
- **ELF64** — `elf::ElfView` decodes little-endian `EM_X86_64` /
  `EM_AARCH64` / `EM_RISCV` files: the file header, program headers, section
  headers, section-name strings, symbol tables (`elf::SymbolTable`), and
  section bytes — enough to name and bound the code regions a disassembler
  walks. ELF extended numbering (`PN_XNUM` / `SHN_LORESERVE` escapes) is
  refused rather than half-decoded; 32-bit and big-endian ELF are not
  recognised, so a caller falls back honestly (to a hex view).
- **wasm** — `wasm::WasmView` decodes a module's section directory and the
  type/function/code framing: `entry_count` reads a vector section's declared
  count, and `code_bodies` walks the function-body boundaries lazily. LEB128
  lengths are strict — more than five bytes, or padding bits set in the final
  byte (the classic overlong-LEB attack), fail closed — and the declared
  bodies must fill the code payload exactly.

`detect` answers "which decoder should look at this file?" from the magic
prefix; the matching decoder still validates everything.

## Lazy, bounded, fail-closed

The inputs are untrusted (any file a user points a viewer at), so every
offset and length is bounds-checked against the input slice before use, and
every table carries a fixed validation cap (`elf::MAX_PROGRAM_HEADERS`,
`elf::MAX_SECTIONS`, `elf::MAX_SYMBOLS`, `elf::MAX_NAME`,
`wasm::MAX_MODULE_SECTIONS`) — security bounds, never growable capacities. A
malformed input is a typed error (`RxeError`, `ElfError`, `WasmError`) naming
what failed; nothing panics, and later bytes are never trusted past a
malformed length.

The ELF and wasm views are *lazy*: parsing validates the header and directory
bounds once, and each program header, section, name, symbol, or function body
decodes on access with its own bounds check — a file with a million-entry
symbol table costs only the entries actually read.

## Sandbox posture

The decoders are parsers of untrusted input, so consumers run them inside the
minimum-capability parser sandbox; they are written as pure, total, `no_std`
slice decoders precisely so they link into that sandbox process unchanged.
The three fuzz harnesses (`fuzz_rxe`, `fuzz_elf`, `fuzz_wasm`) are enrolled
in `cargo xtask fuzz`.
