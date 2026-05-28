# Continuation Prompt — RustOS Stage 3a (c4..c7) + (d) + Stage 2 completion

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

Stages 0–1 and Stage 2.1–2.7 remain complete. Stage 2.8 delivered the
QEMU runner under `tools/qemu` and the two integration test crates
under `tests/integration/{memory_isolation,scheduler_stress_qemu}`.

**Stage 3a (a)** landed the platform-discovery and interrupt-controller
layer in `kernel/arch/x86_64` (`multiboot2`, `acpi`, `apic`,
`apic_timer`, `bootmemory`).

**Stage 3a (b)** added the AP bring-up path: `ap_trampoline.s`,
`smp::TrampolineFrame` / `ApBootSlot` / `init_sipi_sipi`,
identity-map widened to 0..4 GiB, `kernel_main(multiboot_info: u64)`,
and `tests/integration/scheduler_stress_qemu` (cooperative, `-smp 4`).

**Stage 3a (c, partial)** delivered the per-CPU GDT + TSS + IST
*primitives* (`kernel/arch/x86_64/src/gdt.rs`).

**Stage 3a (c1/c2/c3)** — the immediately previous commit — added:

- `kernel/arch/x86_64/src/context.{rs,s}`: `TaskCtx { rsp: u64 }` with
  layout-pinning const-asserts, `TaskCtx::prepare` (synthesises the
  eight-qword resume frame; rejects null / misaligned / too-small
  stacks), and `extern "C" fn rustos_arch_x86_64_switch(prev, next)`
  with a fully-annotated SAFETY-INVARIANT block. Safe wrapper is
  `crate::context::switch`.
- `kernel/arch/x86_64/src/interrupts.{rs,s}`: `InterruptStackFrame`,
  `SavedRegs`, `IdtEntry`, `Idt`, `IdtPointer`, `Idt::load`,
  `Idt::with_default_handler`, and the common ISR prologue
  (`rustos_arch_x86_64_isr_default`) that saves 15 GPRs, aligns the
  stack per SysV §3.2.2, and calls a fail-closed Rust callback
  (`AGENTS.md` §10).
- `kernel/arch/x86_64/src/percpu.rs`: static `[PerCpu; MAX_CPUS=16]`
  arena, `IST_STACK_BYTES = 16 KiB`, `IST_INDEX_DF = 1`,
  `IST_INDEX_NMI = 2`, and `unsafe fn init(cpu_index)` that finalises
  the per-CPU GDT, `lgdt`-installs it, and `lidt`-installs the per-CPU
  IDT. A per-CPU `AtomicBool` latch makes a second `init(i)` return
  `InitError::AlreadyInitialised`.
- `tests/integration/scheduler_stress_qemu` now calls `percpu::init(0)`
  at the top of `kernel_main` and `percpu::init(cpu_id)` at the top of
  each `ap_entry`, retiring the trampoline-internal GDT for
  steady-state. A `const _: () = assert!(MAX_CPUS <= percpu::MAX_CPUS)`
  cross-checks the two sides at compile time.
- 21 new host unit tests (context: 5, interrupts: 8, percpu: 6, plus
  layout const-asserts), taking the arch crate host total to 97.
- Docs: `docs/src/platform/x86_64.md` gained (c1/c2/c3) subsections;
  `PLAN.md` Stage 3a checklist updated.

**Stage 2 is still `in progress`** because the Stage-2 deliverable
sub-checklist (`PLAN.md`) requires LAPIC-timer-driven preemption +
syscall entry + `KernelArch` wiring to flip cleanly to `complete`; the
scheduler_stress_qemu binary today still runs cooperatively (no
preemption — the (c5) work below replaces that).

## Goal of this session

Deliver the remaining Stage-3a x86_64 items below, then flip Stage 2's
status block to `complete`. Scope is x86_64 only; Stages 3b/3c/3d
remain out of scope.

Concretely you must land, to AGENTS.md quality (full `// SAFETY:`
blocks, tests for every invariant, no `unwrap`/`expect`/`panic!`
outside tests and documented boot invariants, no ambient authority):

1. **(c4) `kernel/sched::SchedulerArch` preemption surface.**
   Extend the trait *cleanly* — `SchedulerArch::preempt_to(cpu)` or
   equivalent — with tests + rustdoc in the same commit. Do not bolt
   on a "convenience" wrapper (AGENTS.md §15.5).

2. **(c5) LAPIC-timer-driven preemption.**
   Wire `apic_timer::calibrate` and `program_periodic` into a per-CPU
   init function called from the binary; the IDT vector for the timer
   enters the common prologue and defers into `kernel/sched`. The
   common prologue from (c2) currently has only a fail-closed default
   thunk — extend it via a `define_isr!` macro that emits
   vector-specific stubs (do *not* shoe-horn the timer handler into
   the default thunk; AGENTS.md §15.5).
   Add an assertion to `scheduler_stress_qemu` that the scheduler
   actually preempted at least N times during the run, so a future
   regression that disables preemption fails loudly.

3. **(c6) x86_64 syscall entry.**
   `syscall`/`sysret` MSR programming (`IA32_LSTAR`/`IA32_STAR`/
   `IA32_FMASK`), a kernel stack swap via `IA32_KERNEL_GS_BASE`, and a
   thin entry stub that builds the `RawArgs` the architecture-neutral
   `kernel/syscall::Dispatcher` already validates against
   `SYSCALL_TABLE_HASH`. Do not duplicate any of the syscall-table
   validation surface; reuse it.

4. **(c7) `kernel/core::KernelArch`.**
   Implement against (c1)..(c6) and wire `kernel_main` so a real
   `kernel_main` can boot to the init placeholder. The two QEMU
   integration tests today supply their own `kernel_main`; do not
   break that contract — the new wiring lives in a separate path the
   binary opts into (e.g. a `rustos-kernel` bin crate).

5. **(d1) Per-arch QEMU runner module.**
   Move x86_64-specific defaults out of the generic `tools/qemu::Spec`
   (RAM size, OVMF flags, `isa-debug-exit` device) into a new
   `tools/qemu/src/x86_64.rs`. The generic `Spec` becomes
   architecture-neutral; per-arch modules own the argv assembly. Add
   unit tests for the new module.

6. **(d2) Flip Stage 2 status** in `PLAN.md` to `complete` with the
   same evidence style (toolchain, coverage numbers, `cargo xtask ci`
   tail). Tick sub-checklist items 2.1–2.8 and the Stage 3a
   checklist. Also tick the (c4..c7) and (d1/d2) boxes added by this
   session.

## Hard constraints

- Sensible commit split per AGENTS.md §14, each with
  `Co-authored-by: Junie <junie@jetbrains.com>`. (c4)+(c5) can be a
  single commit if cleanly separable; (c6) and (c7) are typically
  separate; (d1) and (d2) are one commit each.
- `cargo xtask ci` must be green at HEAD of every commit. Quote the
  tail in the final summary.
- Coverage floors per `AGENTS.md` §7: `kernel/arch/x86_64` ≥ 85 %;
  `kernel/mem`, `kernel/sec`, `kernel/ipc`, `lib/caps`, `lib/crypto`
  stay ≥ 95 %.
- No `unwrap`/`expect`/`panic!` in production paths. `unsafe` paired
  with `// SAFETY:` and a test or model. No `#[allow(...)]` without a
  justifying comment.
- Docs in the same commit (AGENTS.md §13):
  `docs/src/platform/x86_64.md` gains (c4..c7) sections;
  `docs/src/architecture/{kernel,memory,scheduler,syscalls}.md`
  updated to reflect the real arch wiring.
- If anything is ambiguous or impossible in one session, **stop and
  ask** (AGENTS.md §15.2 / §15.7) before stubbing. Previous Stage 3a
  sessions honoured this guidance — do the same if the remaining
  surface proves too large for one session.

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
  cooperative loop it uses today; the new assertion that the
  scheduler preempted at least N times during the run is in tree.
- `cargo xtask ci` green; tail quoted in `PLAN.md` Stage 2 status
  block.
- `PLAN.md` Stage 2 marked `complete`; Stage 3a sub-checklist all
  ticked; Stage 3a marked `complete` if every per-arch checklist item
  for x86_64 is satisfied (otherwise leave Stage 3a `in progress` with
  the remaining items honestly enumerated).
- One commit per logical change with the AGENTS.md §14 trailer.
