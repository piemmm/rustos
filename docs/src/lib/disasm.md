# `rustos-disasm` — instruction decoders

`rustos_disasm` (`lib/disasm`) is RustOS's one definition of how machine code
is rendered as text for the four Tier-1 ISAs. The file manager's disassembly
viewer consumes it first (over `lib/binfmt`, which names and bounds the code
regions); an `objdump`-class command app consumes the same crate the moment
it lands. Decoding an instruction is reading bytes, not running code — the
crate needs and grants no authority, executes nothing, and links into the
minimum-capability parser sandbox unchanged (`no_std` + `alloc`,
`#![forbid(unsafe_code)]`, no dependencies).

Stability tier: **experimental** (the opcode tables grow as callers need
them).

## The shared vocabulary

Every ISA module produces the same output type, `Insn`: the instruction's
address, its encoding bytes (capped at `MAX_INSN_BYTES`), its exact byte
`length`, mnemonic and operand text, and the resolved absolute target of a
direct branch/call. A decoder is a **pure function of a byte slice and a
start address** — no state, no I/O — and it always makes forward progress
(length ≥ 1 unit), so a walk over arbitrary bytes terminates and never reads
past the slice.

Undecodable input is rendered honestly and fails closed: `(bad)` over the
unaccounted bytes (one byte on x86_64, so the stream resynchronises exactly
as binutils does; one parcel on riscv64), `.inst 0x…` for an A64 word the
tables do not cover — never a guess, never a panic, never a mis-length.
Mnemonics are canonical encodings, not pseudo-instruction aliases (`addi
zero,zero,0`, not `nop`): a reader inspecting bytes wants the encoding
named, not paraphrased.

## The four decoders

- **`riscv64`** — RV64GC (I, M, A, F, D, Zicsr, Zifencei, C). The 16/32-bit
  parcel-length discipline follows the RISC-V expanded-length encoding
  exactly — the core correctness property, since a mis-lengthed compressed
  instruction desynchronises everything after it. Reserved 48/64-bit
  parcels consume their declared length as `(bad)`.
- **`aarch64`** — fixed-width A64: PC-relative addressing (`adr`/`adrp`),
  add/sub and logical (bitmask, `DecodeBitMasks`) immediates, move-wide,
  bitfield/extract, every branch form, exception generation and hints,
  loads/stores (register, pair, literal, exclusive/acquire-release), and
  the data-processing register families. SIMD/FP data processing is
  summarised as `.inst` with full operand decode staged
  (`.junie/fstree-next-plan.md`).
- **`wasm`** — the structured opcode stream of one code-section body.
  Nesting is a property of the surrounding stream, so `decode` takes the
  current block depth and returns the next one, rendering nesting as
  indentation (clamped at `MAX_INDENT_LEVELS`). LEB128 immediates are
  strict — overlong padding fails closed — and a hostile `br_table` count
  is refused at `MAX_BR_TABLE_TARGETS` (validation bounds, not growable
  capacities). Offsets stand in for addresses; branch labels are relative
  label indices, so `branch_target` stays empty.
- **`x86_64`** — the variable-length decoder: legacy prefixes, REX,
  one-byte and `0F` two-byte opcode maps, ModRM/SIB, displacement and
  immediate sizing, the architectural 15-byte cap, rendered in
  binutils-style Intel syntax. `0F 38`/`0F 3A` and VEX/EVEX are staged and
  render `(bad)` until then.

## Testing

Each module carries a byte-for-byte conformance table of hand-assembled
encodings (mnemonic, operands, length, branch target), plus fail-closed
tests for truncation, reserved encodings, and the validation bounds. Four
fuzz harnesses (`fuzz_riscv64`, `fuzz_aarch64`, `fuzz_wasm_isa`,
`fuzz_x86_64`) are enrolled in `cargo xtask fuzz` and assert the
never-panics and forward-progress invariants over random and
template-mutated streams.
