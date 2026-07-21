# FIX-PANICS — Verbose kernel-panic post-mortem (registers + backtrace)

Status: **planned** (not started)

Binding under `AGENTS.md`. This plan makes a kernel panic dump a rich,
structured post-mortem — a register snapshot and a bounded stack
backtrace — instead of the current single `PanicInfo` line, and in doing
so **collapses the three divergent per-arch panic bridges onto the one
arch-neutral `panic_dump` path**. The refactor is a net reduction in
duplicated code (§2.2), not an addition.

Read first (§15.18): `plans/WIRING.md` (Arch HAL parity — this adds a new
closed HAL slice), `plans/WATCHDOG.md` (the existing per-CPU liveness /
`CpuState` machinery and the non-maskable-sample pattern a post-mortem
reuses), `docs/src/architecture/scheduler.md` and the arch-port docs, and
`kernel/core/src/panic.rs` / `kernel/core/src/audit.rs` (the current
path and the `AuditEvent::Panic` contract).

## The observed defect

A panic today emits one line, e.g. an OOM:

```
==================== TAIRiX KERNEL PANIC ====================
[tairix-kernel] aarch64 panic on CPU 0: panicked at .../alloc.rs:573:9:
memory allocation of 16777216 bytes failed
CPU 0 halted; the kernel is non-recoverable in production.
=============================================================
```

Two problems:

1. **Not enough for a post-mortem.** No registers, no backtrace — a fatal,
   non-recoverable, halting event should carry everything a post-mortem
   needs.
2. **Three divergent implementations.** `x86_64` routes its
   `#[panic_handler]` through `tairix_kernel_core::handle_panic` →
   `panic_dump` (the structured, audited path). `aarch64`
   (`handle_panic_via_serial`) and `riscv64` each have their **own**
   simpler serial-banner bridge that deliberately does **not** route
   through `kernel_core`. Three hand-written panic bodies emitting three
   slightly different banners is the duplication the charter forbids
   (§2.2); adding a backtrace to each would triple it.

## The "which images?" decision (charter-derived)

The user's rule: verbose panic is debug-only **unless there is no CPU
overhead, in which case every image gets it.** Applying it:

- **Registers + raw (hex-address) backtrace → EVERY image.** There is no
  steady-state CPU cost. Frame pointers are **already** forced on all
  three bare-metal targets (`.cargo/config.toml` carries
  `-C force-frame-pointers=yes` for `x86_64-unknown-none`,
  `aarch64-unknown-none`, and `riscv64gc-unknown-none-elf`), so the
  frame-pointer chain is already maintained at runtime. Walking it costs a
  handful of loads, and only at panic time, when the CPU is halting anyway.
  Reading the GP registers is a read-only snapshot. Neither adds a cycle to
  any hot path.
- **On-target symbolication (turn `0xffff…1234` into `fn_name+0x40`) →
  DEBUG images only.** Its cost is a *size* cost (a symbol table shipped in
  the image), not a CPU cost. Gate it on `cfg!(debug_assertions)`. The
  zero-image-cost default everywhere is to print **raw addresses** and
  resolve them offline against the unstripped kernel ELF with
  `addr2line` / `llvm-symbolizer` — exactly how Linux's
  `decode_stacktrace.sh` works.

The debug gate already exists honestly: `ImageProfile::Debug` builds the
kernel **without** `--release` and `ImageProfile::Installer` builds **with**
`--release` (`tools/xtask/src/commands.rs::kernel_build_profile`), so
`cfg!(debug_assertions)` is the natural gate for the symbol-table path.

## Design

### 1. New closed Arch HAL slice: `tairix_arch_api::backtrace`

Capturing registers and unwinding one stack frame is genuinely
target-divergent (register file, ABI frame layout, the privileged reads) —
the textbook Arch HAL case (§17.2), not per-arch copy-paste (§2.20, §2.21).
Add `kernel/arch/api/src/backtrace.rs` alongside `sidechannel.rs` /
`memtag.rs`, exported from `kernel/arch/api/src/lib.rs`, with a
`backtrace::conformance` vertical like every other slice.

```rust
/// Read-only CPU register snapshot for a post-mortem dump.
pub trait CpuStateCapture {
    /// Snapshot the current GP registers + PC/SP/FP. No side effects,
    /// no allocation.
    fn capture(&self) -> RegisterSnapshot;

    /// Given a (frame-pointer, program-counter) pair, return the
    /// caller's (fp, pc), or `None` when the chain ends or fails a
    /// validity check. One frame per call — the neutral walker loops.
    fn unwind_one(&self, frame: StackFrame) -> Option<StackFrame>;

    /// Honest capability report, mirroring `TaggingProfile`:
    /// `Supported` / `Unsupported(reason)`.
    fn profile(&self) -> BacktraceProfile;
}
```

- The **register read** and the **single-frame unwind rule** (which
  register is the fp, how the saved-fp/saved-lr pair is laid out) are the
  *only* arch-specific parts, implemented in each `kernel/arch/<target>/`.
- Register reads live in the arch crate via the §1 inline-asm carve-out,
  each with the mandated `// SAFETY:` header.
- `x86_64` / `aarch64` / `riscv64` implement it honestly; `wasm32` returns
  `Unsupported("host-managed stack; traps to the JS harness")` — a
  documented honest `Unsupported`, **never** a faked no-op (matches the
  existing `TaggingProfile` / side-channel pattern; satisfies §19.10's "no
  no-op" bar).
- `RegisterSnapshot`, `StackFrame`, `BacktraceProfile` are `lib/abi`- /
  HAL-level types held to the §17.2 discipline.

### 2. The unwinder is arch-neutral and lives in `kernel/core`

Everything except "read a register" and "decode one frame" is identical
across arches, so it MUST be shared (§2.21). Extend `panic_dump` in
`kernel/core/src/panic.rs`:

- Ask the arch for a `RegisterSnapshot`, format it into stack buffers
  (add a `format_hex_u64` beside the existing `format_u32`), and emit it
  as audit fields.
- Loop `unwind_one` up to a **fixed cap** (64 frames), emitting each `pc`
  as a hex audit field (`frame_0`, `frame_1`, …), then halt.

### 3. Route all three arches through `kernel_core`

- Add `backtrace: Option<&dyn CpuStateCapture>` to `PanicContext` (borrowed
  read-only, no locks on the panic path).
- Replace `handle_panic_via_serial` (aarch64) and the riscv64 equivalent
  with a one-line `#[panic_handler]` forwarding to
  `kernel_core::handle_panic`, exactly like x86_64 already does. Delete the
  bespoke banner bodies (§2.14 — delete dead code).
- The aarch64/riscv64 bridges' stated reason for not routing through core
  ("no post-init arch handle to publish") is resolved the same way x86_64
  solved it: publish the arch handle into an `AtomicPtr` at boot (see
  `x86_64/panic_ctx.rs::publish_arch`), with the pre-init null path emitting
  the minimal one-liner. Hoist that publish-arch-ptr pattern into shared
  code if it is now identical across all three arches (§2.21).

## Correctness constraints (the Linus bar — non-negotiable)

A stack unwinder in a panic handler that faults itself is a triple-fault.

1. **No allocation on the panic path.** The OOM example *is* a heap
   failure — the handler must never touch the heap. Everything is
   stack-buffered, as `panic_dump` already is. `capture` and `unwind_one`
   allocate nothing (§4, §2.9).
2. **Validate every frame pointer before dereferencing it — fail closed,
   stop the walk, never fault.** Each candidate fp must be: non-null,
   correctly aligned, strictly greater than the previous fp (monotonic —
   kills cycles), and within the current CPU's kernel stack bounds. Any
   failure ends the walk cleanly. This is the crux; a naive `*(fp)` walk on
   a corrupt chain is a fault-in-fault-handler.
3. **Bounded depth.** Hard cap of 64. A corrupt-but-plausible chain must
   terminate.
4. **Re-entrancy guard.** A per-CPU "already panicking" flag: a panic
   *inside* the panic handler emits one terse line and halts immediately,
   no recursion. (This is a latent gap in the current handler regardless of
   backtracing — fix it here.)
5. **Frame-pointer walk only — no DWARF `.eh_frame` / CFI machinery.** The
   kernel is `panic = abort` with `build-std` and no unwinder; forced frame
   pointers make the fp-walk correct and cheap and avoid shipping/parsing
   CFI (bigger attack surface, real cost). Keep it simple.
6. **Kernel-stack bounds come from the port.** The walk needs the current
   CPU's kernel-stack `[base, top)`; source it from the per-CPU state the
   watchdog / SMP bring-up already tracks (`plans/WATCHDOG.md`,
   `kernel/core::cpu_state`), not a fresh global.

## Policy / ABI decisions to make explicitly

- **Address-leak policy.** `AuditEvent::TaskFaultKilled`
  (`kernel/core/src/audit.rs`) deliberately omits the raw *user* faulting
  address (no ASLR/layout leak onto the log). A *kernel* panic dump (fatal,
  non-recoverable, halting) legitimately prints kernel addresses — but
  decide this **explicitly** and document it; do not silently contradict
  the existing user-fault policy. Panic dumps carry kernel addresses;
  user-fault kills still do not.
- **Audit contract change.** `AuditEvent::Panic`'s field set
  (`cpu` / `file` / `line` / `column`) is part of the audit contract
  asserted by tests (`kernel/core/src/panic.rs` tests +
  `tests/integration/kernel_arch_boot*`). Adding `registers` and `frame_N`
  fields changes that contract — fine, the ABI is not frozen (§2.13: evolve
  in place, update every caller in the same change). Update those tests;
  do **not** add a `Panic` v2.

## Deliverables (single change, done fully — no no-ops, no stubs)

- `kernel/arch/api/src/backtrace.rs`: the `CpuStateCapture` trait,
  `RegisterSnapshot`, `StackFrame`, `BacktraceProfile`, plus a
  `backtrace::conformance` vertical. Export from
  `kernel/arch/api/src/lib.rs`; record the new HAL slice per §17.2 (PLAN.md
  entry + `plans/WIRING.md` status row).
- `kernel/arch/{x86_64,aarch64,riscv64}/`: implement the trait honestly
  (asm register read + one-frame unwind + stack-bounds check). `wasm32`
  returns the honest `Unsupported`.
- `kernel/core/src/panic.rs`: extend `panic_dump` with the register block,
  the bounded fp-walk, the re-entrancy guard, `format_hex_u64`, and the
  `cfg!(debug_assertions)`-gated symbolication hook.
- `kernel/arch/aarch64/src/panic.rs` + the riscv64 equivalent: delete the
  bespoke `handle_panic_via_serial` banner path; route through
  `kernel_core::handle_panic` via the publish-arch-ptr pattern (§2.14).
- **Symbolication (debug only):** first cut is offline — raw addresses
  everywhere, resolved with `addr2line` against the debug ELF (add a
  `tools/` helper or document the exact invocation, mirroring Linux's
  `decode_stacktrace.sh`). On-target names, if wanted later, embed a
  compact sorted `(addr, name)` table in a dedicated section, looked up by
  binary search on the panic path, `cfg!(debug_assertions)`-gated. **Stage
  the on-target part in `PLAN.md`** rather than half-building it (§2.19 —
  do the offline cut fully, do not ship a partial on-target lookup).

## Tests (§7 — part of this change)

- **Host unit tests** for the neutral walker driving a mock
  `CpuStateCapture`: normal chain, cycle (non-monotonic fp), unaligned fp,
  null fp, out-of-bounds fp, depth cap. Plus `format_hex_u64` and the
  re-entrancy guard.
- **`backtrace::conformance` vertical** per arch under QEMU (register
  capture non-trivial, one-frame unwind sane, `profile()` honest).
- **Updated end-to-end panic vertical** (`tests/integration/kernel_arch_boot*`)
  asserting the new banner shows the register block and ≥ 2 backtrace
  frames, and that `AuditEvent::Panic` now carries the `registers` /
  `frame_N` fields.
- A **fuzz harness** for the frame-pointer walker fed adversarial
  fp/pc chains (§19.6): it must always terminate and never dereference an
  invalid pointer.

## Docs (§2.8, §13)

- Rustdoc on the new HAL slice and every public type.
- A `docs/src/architecture/panic-diagnostics.md` page: what a panic dump
  contains, the every-image vs debug-only split, the address-leak policy,
  and the offline `addr2line` symbolication workflow.
- Update the arch-port docs and, if a support-matrix row is warranted, the
  `README.md` matrix (§13).

## Definition of done

The §2.15 / §7 whole-project gate is green (`cargo fmt --all`, one
`cargo xtask ci`, `cargo xtask fuzz --secs 5`, the `tools/ci/soak.sh both
--secs 20` smoke), output quoted in the completion report, and the §23
self-review verdict stated. Every image emits registers + a hex backtrace;
debug images additionally resolve symbols; the three per-arch panic bridges
are gone, replaced by one shared `panic_dump` path.
