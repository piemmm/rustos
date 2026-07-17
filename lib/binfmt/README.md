# tairix-binfmt

Read-only executable-container decoder for TAIRiX (`lib/binfmt`).

Several TAIRiX components need to *look inside* an executable file without
loading it — the file manager's disassembly viewer first, an
`objdump`/`readelf`-class command app next. That decoding is identical
wherever it happens, so it lives here once and every consumer imports it.

## What it decodes

- **`rxe`** — the TAIRiX load image (`rxe::RxeView`: header, W^X-clean
  segment table, needed libraries, entry point) and the signed manifest
  summary (`rxe::ManifestSummary`: header plus requested capabilities).
  Both decode *through* the `lib/abi` wire types
  (`LoadImage::parse_for_inspection`, `ManifestHeader::from_bytes`,
  `decode_capability_ids`), so the inspection view and the kernel load path
  share one definition and cannot drift. The CFI tag is reported, never
  compared — admitting a binary to execution stays the kernel loader's job.
- **ELF64** — little-endian `EM_X86_64` / `EM_AARCH64` / `EM_RISCV` files
  (`elf::ElfView`): file header, program headers, section headers,
  section-name strings, and symbol tables — enough to name and bound the
  code regions a disassembler walks. The view is lazy: parse validates the
  header and table bounds once; each entry decodes on access with its own
  bounds check, so a huge symbol table costs only the entries actually read.
- **wasm** — module structure (`wasm::WasmView`): the section directory and
  the type/function/code section framing (function-body boundaries), with
  strictly validated LEB128 lengths (overlong encodings fail closed).

`detect` recognises which decoder should look at a byte prefix.

## What it never does

The decoders are **read-only decoders of untrusted input**: they never
execute, map, or load anything. Every offset and length is bounds-checked
against the input slice before use; every table carries a fixed validation
cap (`elf::MAX_PROGRAM_HEADERS`, `elf::MAX_SECTIONS`, `elf::MAX_SYMBOLS`,
`elf::MAX_NAME`, `wasm::MAX_MODULE_SECTIONS` — security bounds, not
growable capacities); a malformed input is a typed error naming what
failed, never a panic and never a partial trust of later bytes.

Consumers run these decoders inside the minimum-capability parser sandbox;
the functions here are pure, total, `no_std` slice decoders precisely so
they link into that sandbox process unchanged.

## Design

- `no_std` + `alloc`, `#![forbid(unsafe_code)]`.
- One dependency: `tairix-abi` (the rxe/manifest wire types this crate
  reads through).
- Fuzzed: `fuzz_rxe`, `fuzz_elf`, and `fuzz_wasm` are enrolled in
  `cargo xtask fuzz`.

## Stability

Tier: `experimental`.
