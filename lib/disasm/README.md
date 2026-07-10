# rustos-disasm

Instruction decoders for RustOS (`lib/disasm`).

The file manager's disassembly viewer — and an `objdump`-class command app
next — need to render machine code as text for the four Tier-1 ISAs. That
decoding is identical wherever it happens, so it lives here once, one
module per ISA, all producing the one shared output vocabulary (`Insn`:
address, encoding bytes, length, mnemonic, operands, branch target).

## What it decodes

- **riscv64** — RV64GC (I, M, A, F, D, Zicsr, Zifencei, and the C
  compressed extension). The 16/32-bit parcel-length discipline follows
  the RISC-V expanded-length encoding exactly, so a compressed
  instruction never swallows its successor; reserved 48/64-bit parcels
  consume their declared length as `(bad)`.
- **aarch64** — fixed-width A64: PC-relative addressing, add/sub and
  logical (bitmask) immediates, move-wide, bitfield/extract, every branch
  form, exception generation and hints, loads/stores (register, pair,
  literal, exclusive/acquire-release), and the data-processing register
  families. An encoding outside the tables renders honestly as
  `.inst 0x…` — never skipped, never guessed; SIMD/FP data processing is
  summarised that way with full operand decode staged
  (`.junie/fstree-next-plan.md`).
- **wasm** — the structured opcode stream of a code-section body (the
  bytes `lib/binfmt`'s `code_bodies` walk frames): block nesting rendered
  by indentation, strict LEB128 immediates (overlong encodings fail
  closed), the `0xFC` prefixed set, and a `br_table` target bound.
- **x86_64** — the variable-length decoder: legacy prefixes, REX,
  ModRM/SIB, displacement/immediate sizing over the one- and two-byte
  opcode maps, rendered in binutils-style Intel syntax. An undecodable
  byte is a `(bad)` single byte so the stream resynchronises exactly as
  binutils does; `0F 38`/`0F 3A` and VEX/EVEX are staged.

Every decoder is a pure function of a byte slice and a start address: it
always makes forward progress (so a walk over any input terminates),
never reads past the slice, and never executes or interprets anything.

## Hardening

These decoders parse untrusted executable-file bytes. Malformed input
renders as `(bad)`/`.inst` — never a panic, never a mis-length. Counts
that ride the input are validation-bounded (`wasm::MAX_BR_TABLE_TARGETS`,
`wasm::MAX_INDENT_LEVELS`, the x86_64 15-byte instruction cap — security
bounds, not growable capacities). Consumers run the decoders inside the
minimum-capability parser sandbox; the functions here are pure, total,
`no_std` slice decoders precisely so they link into that sandbox process
unchanged.

## Design

- `no_std` + `alloc`, `#![forbid(unsafe_code)]`, no dependencies.
- Canonical mnemonics, not pseudo-instruction aliases: a reader
  inspecting bytes wants the encoding named, not paraphrased.
- Fuzzed: `fuzz_riscv64`, `fuzz_aarch64`, `fuzz_wasm_isa`, and
  `fuzz_x86_64` are enrolled in `cargo xtask fuzz`.

## Stability

Tier: `experimental`.
