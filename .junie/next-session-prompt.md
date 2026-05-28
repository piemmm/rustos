# Continuation Prompt — RustOS Stage 3a (b)/(c)/(d) + Stage 2 completion

Copy the text below verbatim into the next agent session as the
`<issue_description>`.

---

Read `AGENTS.md` and `PLAN.md` in full before doing anything else. They
are binding. Do not skim. In particular: §2 (no hacks, no duplication,
no bloat, no interface creep), §7 (tests must pass; no `#[ignore]`;
coverage floors), §10 (`unsafe` requires `// SAFETY:` + a test + safe
encapsulation), §14 (commit format, one logical change per commit), §15
(no stubs, no silenced lints, no weakened security, no invented APIs).

## Context (already on `main`)

Stages 0–1 and Stage 2.1–2.7 remain complete. Stage 2.8 delivers the
QEMU runner under `tools/qemu`, the `cargo xtask test --qemu` flag, and
the two integration test crates under
`tests/integration/{memory_isolation,scheduler_stress}`.

The most recent commit, **Stage 3a (a)**, landed the platform-discovery
and interrupt-controller layer in `kernel/arch/x86_64`:

- `multiboot2`: zero-copy parser for the Multiboot2 information
  structure (tags 4, 6, 14, 15, 17).
- `acpi`: RSDP v1/v2 checksum validation, generic SDT header validator,
  typed MADT iterator (Local APIC, IO-APIC, Interrupt Source Override,
  LAPIC NMI, LAPIC Address Override).
- `apic`: `Lapic` / `IoApic` drivers behind `LapicMmio` / `IoApicMmio`
  traits with a volatile-MMIO production impl and host-side mocks;
  software-enable, EOI, INIT/SIPI/INIT-deassert IPI sequence, IO-APIC
  redirection-entry programming.
- `apic_timer`: PIT-channel-2 calibration into a `Calibration { ... }`
  value and periodic-mode LAPIC timer programming.
- `bootmemory`: bridge from Multiboot2 / UEFI memory-map entries to
  `MemoryRegionDescriptor`s; a host-side dev-dep round-trip test locks
  the local `RegionKind` mirror against `rustos_kernel_mem::RegionKind`
  (AGENTS.md §2.2 — no duplication).

All five modules are `no_alloc` in production so the freestanding
Stage-2 QEMU test binaries still link. 47 host unit tests cover the new
code; `cargo xtask ci` is green on `nightly-2026-05-27` /
rustc 1.98.0-nightly with QEMU 8.2.2, `grub-mkrescue`, `xorriso`, OVMF
2024.02.

**Stage 2 is still `in progress`** because the deliverable text on
`PLAN.md` lines 154–158 mandates the scheduler stress test run *under
QEMU on ≥ 4 emulated cores*; today it runs host-side against
`kernel/sched::TestArch`. Stage 2 only flips to `complete` once the
items below ship.

## Goal of this session

Deliver the remaining Stage-3a x86_64 items enumerated in `PLAN.md`
§Stage 3 → 3a "Remaining for Stage 3a completion" — concretely commits
(b), (c), (d) below — then flip Stage 2's status block to `complete`.
Scope is x86_64 only; Stages 3b/3c/3d remain out of scope.

Concretely you must land, to AGENTS.md quality (full `// SAFETY:`
blocks, tests for every invariant, no `unwrap`/`expect`/`panic!`
outside tests and documented boot invariants, no ambient authority):

1. **(b) AP bring-up.** INIT-SIPI-SIPI sequence at a 4 KiB-aligned low
   physical-memory trampoline, reusing the `Lapic::send_init` /
   `send_sipi` primitives already in `apic.rs`. Promote
   `tests/integration/scheduler_stress` to a QEMU binary running on
   `-smp 4`. The host-side workspace stress test stays — **both must
   pass**.
2. **(c) Context switch + interrupt entry/exit prologue** matching
   `kernel/sched::SchedulerArch`. Wire the calibrated LAPIC timer into
   `kernel/sched`'s preemption hook.
3. **(c) x86_64 syscall entry stub** bound to
   `kernel/syscall::Dispatcher` via `syscall`/`sysret`. The
   architecture-neutral dispatcher already validates against
   `SYSCALL_TABLE_HASH` — reuse it, do not duplicate.
4. **(c) Implement `kernel/core::KernelArch`** against the above and
   wire `kernel_main`.
5. **(d) Per-arch QEMU runner module** `tools/qemu/src/x86_64.rs`; move
   the x86_64-specific defaults out of the generic `Spec` in `lib.rs`.
6. **(d) Flip Stage 2 status** in `PLAN.md` to `complete` with the same
   evidence style (toolchain, coverage numbers, `cargo xtask ci` tail).
   Tick sub-checklist items 2.1–2.8 and the Stage 3a checklist.

## Hard constraints

- Sensible commit split per AGENTS.md §14, each with
  `Co-authored-by: Junie <junie@jetbrains.com>`:
    - **(b)** AP bring-up + `scheduler_stress` QEMU promotion.
    - **(c)** Context switch + interrupt prologue + syscall entry +
      `KernelArch` wiring.
    - **(d)** QEMU runner refactor + `PLAN.md` Stage 2 flip.
- `cargo xtask ci` must be green at HEAD of every commit. Quote the
  tail in the final summary.
- Coverage floors per `AGENTS.md` §7: `kernel/arch/x86_64` ≥ 85 %;
  `kernel/mem`, `kernel/sec`, `kernel/ipc`, `lib/caps`, `lib/crypto`
  stay ≥ 95 %.
- No `unwrap`/`expect`/`panic!` in production paths. `unsafe` paired
  with `// SAFETY:` and a test or model. No `#[allow(...)]` without a
  justifying comment.
- **No invented APIs.** If you need new surface in `kernel/mem` /
  `kernel/sched` / `kernel/syscall` / `kernel/core` (in particular a
  `SchedulerArch` preemption hook), propose it in `PLAN.md` first or
  add it cleanly with tests and rustdoc in the same commit; do not
  bolt on "convenience" wrappers (AGENTS.md §15.5).
- Docs in the same commit (AGENTS.md §13):
  `docs/src/platform/x86_64.md` loses the remaining Stage-3a caveats
  where appropriate;
  `docs/src/architecture/{kernel,memory,scheduler,syscalls}.md`
  updated to reflect the real arch wiring.
- If anything is ambiguous or impossible in one session, **stop and
  ask** (AGENTS.md §15.2 / §15.7) before stubbing. The previous
  session honoured this guidance and split the original 8-item brief
  into (a) (landed) and (b)/(c)/(d) (deferred to this prompt) rather
  than ship half-finished code; do the same if the remaining surface
  proves too large for one session.

## Toolchain & host requirements (already installed on the workbench)

- `nightly-2026-05-27` toolchain (rustc 1.98.0-nightly).
  PATH: `$HOME/.rustup/toolchains/nightly-2026-05-27-x86_64-unknown-linux-gnu/bin`.
- `qemu-system-x86_64` 8.2.2, `grub-mkrescue`, `xorriso`,
  `/usr/share/OVMF/OVMF_CODE_4M.fd` + `OVMF_VARS_4M.fd`, `mdbook`,
  `cargo-deny`, `cargo-llvm-cov` (in `~/.cargo/bin`).
- `mdbook` lives in `~/.cargo/bin`. Make sure that directory is on
  `PATH` *before* invoking `cargo xtask ci` — `xtask` does not search
  it automatically.

## Definition of done

- All remaining Stage-3a items above implemented to AGENTS.md quality.
- `tests/integration/scheduler_stress` runs under QEMU on `-smp 4`
  through `cargo xtask test --qemu` with no retries and a strict
  timeout.
- `cargo xtask ci` green; tail quoted in `PLAN.md` Stage 2 status
  block.
- `PLAN.md` Stage 2 marked `complete`; Stage 3a sub-checklist all
  ticked; Stage 3a marked `complete` if every per-arch checklist item
  for x86_64 is satisfied (otherwise leave Stage 3a `in progress` with
  the remaining items honestly enumerated).
- One commit per logical change with the AGENTS.md §14 trailer.
