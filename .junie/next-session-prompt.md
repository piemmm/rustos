# Continuation Prompt — RustOS Stage 3a (c)/(d) + Stage 2 completion

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

**Stage 3a (c) — partial.** The first slice of (c) is now on `main`:
`kernel/arch/x86_64/src/gdt.rs` ships the per-CPU GDT + TSS + IST
*primitives* (canonical 7-slot layout, `GdtEntry` constructors,
`tss_descriptor` splitter, SDM-aligned `Tss`, `PerCpuGdt` builder with
`set_ist` / `set_privilege_stack` / `finalize`, an `unsafe fn
install(&'static mut self)` gated to `target_os = "none"`). The slice
has 21 dedicated host unit tests, full rustdoc on every public item,
and a Stage 3a (c, partial) section in `docs/src/platform/x86_64.md`.
**Not yet wired** into either the BSP or the APs in
`tests/integration/scheduler_stress_qemu` — that's part of the remaining
work below.

**Stage 3a (a)** landed the platform-discovery and interrupt-controller
layer in `kernel/arch/x86_64` (`multiboot2`, `acpi`, `apic`,
`apic_timer`, `bootmemory`).

**Stage 3a (b)** — the immediately previous commit — added the
application-processor bring-up path:

- `kernel/arch/x86_64/src/ap_trampoline.s`: position-independent
  real-mode → long-mode payload, hardcoded landing at
  `AP_TRAMPOLINE_PHYS = 0x8000` (SIPI vector `0x08`).
- `kernel/arch/x86_64/src/smp.rs`: `TrampolineFrame` installer,
  `ApBootSlot`, `init_sipi_sipi` sequencer over `Lapic::send_ipi` /
  `send_init_deassert`. 9 new host unit tests cover layout / install /
  ordering.
- `boot.s` extended to identity-map the full 0..4 GiB window so LAPIC /
  IO-APIC / ACPI tables are reachable.
- `kernel_main(multiboot_info: u64)` signature: the BSP boot trampoline
  now hands the verbatim multiboot info pointer to the binary.
- `tests/integration/scheduler_stress_qemu/`: sibling crate to the host
  `scheduler_stress`, a freestanding x86_64 kernel that parses
  Multiboot2 → RSDP → XSDT/RSDT → MADT, brings up 3 APs, and drives
  8 192 tasks across 4 real (emulated) cores cooperatively. Runs under
  `cargo xtask test --qemu` with a 120 s timeout. Host-side
  `scheduler_stress` is unchanged and still green.

**Stage 2 is still `in progress`** because the Stage-2 deliverable
sub-checklist (`PLAN.md`) requires Stage 3a's context switch + interrupt
prologue + LAPIC-timer preemption to flip cleanly to `complete`; today
the scheduler_stress_qemu binary runs *cooperatively* (no preemption).
The previous session split the original Stage-3a brief into (a)
landed, (b) landed, and (c)/(d) deferred to this prompt.

## Goal of this session

Deliver the remaining Stage-3a x86_64 items enumerated in `PLAN.md`
§Stage 3 → 3a — concretely commits (c) and (d) below — then flip
Stage 2's status block to `complete`. Scope is x86_64 only; Stages
3b/3c/3d remain out of scope.

Concretely you must land, to AGENTS.md quality (full `// SAFETY:`
blocks, tests for every invariant, no `unwrap`/`expect`/`panic!`
outside tests and documented boot invariants, no ambient authority):

1. **(c) Context-switch primitive + interrupt entry/exit prologue.**
   - **Wire** `kernel::arch::x86_64::gdt::PerCpuGdt::install` from each
     AP (and the BSP) inside
     `tests/integration/scheduler_stress_qemu/src/kernel.rs`,
     replacing the trampoline-internal GDT. The *primitives* are
     already on `main`; only the wiring + per-AP static storage + IST
     stack arenas remain.
   - Common ISR prologue / epilogue (save GPRs + FXSAVE area + segment
     swap, RIP/CS/RFLAGS/RSS/RSP from the CPU-pushed frame).
   - Context-switch primitive `extern "C" fn switch(prev: *mut TaskCtx,
     next: *mut TaskCtx)` matching what
     `kernel/sched::SchedulerArch` needs to actually preempt. Add the
     `SchedulerArch::preempt_to(cpu)` (or equivalent) surface in
     `kernel/sched` *cleanly* (tests + rustdoc in the same commit) —
     do not bolt on a "convenience" wrapper (AGENTS.md §15.5).
   - LAPIC-timer-driven preemption: wire `apic_timer::calibrate` and
     `program_periodic` into a per-CPU init function called from the
     binary; the IDT vector for the timer enters the prologue and
     defers into `kernel/sched`.

2. **(c) x86_64 syscall entry.** `syscall`/`sysret` programming
   (`IA32_LSTAR`/`IA32_STAR`/`IA32_FMASK`), a kernel stack swap via
   `IA32_KERNEL_GS_BASE`, and a thin entry stub that builds the
   `RawArgs` the architecture-neutral `kernel/syscall::Dispatcher`
   already validates against `SYSCALL_TABLE_HASH`. Do not duplicate
   any of the syscall-table validation surface; reuse it.

3. **(c) `kernel/core::KernelArch`.** Implement against (1)+(2) and
   wire `kernel_main` so a real `kernel_main` can boot to the init
   placeholder. The two QEMU integration tests today supply their own
   `kernel_main`; do not break that contract — the new wiring lives in
   a separate path the binary opts into (e.g. a `rustos-kernel`
   bin crate).

4. **(d) Per-arch QEMU runner module.** Move x86_64-specific defaults
   out of the generic `tools/qemu::Spec` (RAM size, OVMF flags,
   `isa-debug-exit` device) into a new `tools/qemu/src/x86_64.rs`. The
   generic `Spec` becomes architecture-neutral; per-arch modules own
   the argv assembly. Add unit tests for the new module.

5. **(d) Flip Stage 2 status** in `PLAN.md` to `complete` with the same
   evidence style (toolchain, coverage numbers, `cargo xtask ci`
   tail). Tick sub-checklist items 2.1–2.8 and the Stage 3a
   checklist. Also tick the (c) and (d) boxes added by this session.

## Hard constraints

- Sensible commit split per AGENTS.md §14, each with
  `Co-authored-by: Junie <junie@jetbrains.com>`:
    - **(c)** Context switch + interrupt prologue + syscall entry +
      `KernelArch` wiring. May be split into 2–3 commits if cleanly
      separable; the per-CPU GDT/TSS commit can land before the
      syscall commit.
    - **(d)** QEMU runner refactor + `PLAN.md` Stage 2 flip.
- `cargo xtask ci` must be green at HEAD of every commit. Quote the
  tail in the final summary.
- Coverage floors per `AGENTS.md` §7: `kernel/arch/x86_64` ≥ 85 %;
  `kernel/mem`, `kernel/sec`, `kernel/ipc`, `lib/caps`, `lib/crypto`
  stay ≥ 95 %.
- No `unwrap`/`expect`/`panic!` in production paths. `unsafe` paired
  with `// SAFETY:` and a test or model. No `#[allow(...)]` without a
  justifying comment.
- **No invented APIs.** Adding a preemption surface to
  `kernel/sched::SchedulerArch` is explicitly *not* forbidden — the
  current `SchedulerArch` (just `current_cpu` / `ticks_now` /
  `send_ipi`) is intentionally minimal and the prompt acknowledges
  this is the moment to extend it. Extend it *cleanly*, with tests +
  rustdoc; do not bolt on convenience wrappers.
- Docs in the same commit (AGENTS.md §13):
  `docs/src/platform/x86_64.md` Stage 3a (c) section;
  `docs/src/architecture/{kernel,memory,scheduler,syscalls}.md`
  updated to reflect the real arch wiring.
- If anything is ambiguous or impossible in one session, **stop and
  ask** (AGENTS.md §15.2 / §15.7) before stubbing. The Stage 3a (a)
  and Stage 3a (b) sessions honoured this guidance — do the same if
  the remaining surface proves too large.

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
- `tests/integration/scheduler_stress_qemu` continues to pass on
  `-smp 4`, now with real LAPIC-timer-driven preemption instead of the
  cooperative loop it uses today; add a new assertion that the
  scheduler actually preempted at least N times during the run, so a
  future regression that disables preemption fails loudly.
- `cargo xtask ci` green; tail quoted in `PLAN.md` Stage 2 status
  block.
- `PLAN.md` Stage 2 marked `complete`; Stage 3a sub-checklist all
  ticked; Stage 3a marked `complete` if every per-arch checklist item
  for x86_64 is satisfied (otherwise leave Stage 3a `in progress` with
  the remaining items honestly enumerated).
- One commit per logical change with the AGENTS.md §14 trailer.
