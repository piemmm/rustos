# Continuation Prompt — RustOS Stage 3a (c6/c7) + (d1/d2)

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

**Stage 3a (c1/c2/c3)** added per-CPU GDT+IDT bring-up
(`percpu::init`), the `TaskCtx` context-switch primitive, the common
fail-closed ISR prologue, and the corresponding host unit tests.

**Stage 3a (c4)** — the *previous* session — added the scheduler-side
preemption observation point:
`Scheduler::on_timer_tick(cpu) -> SchedResult<()>` plus the per-CPU
counters `Scheduler::preemption_count(cpu)` and
`Scheduler::total_preemption_count()`. The entry point is *counter-
only* (bumps a `Relaxed` `AtomicU64` and returns) because the task
registry `RwLock` and the overflow `SpinLock` are explicitly
forbidden from interrupt context by `kernel/sync`. The cooperative
`step` loop driven from kernel-thread context remains the only writer
of run-queue state. The `SchedulerArch` trait is deliberately not
extended (`send_ipi` already documents the scheduler-asks-arch
direction; a parallel `preempt_to` would be §2.4 interface creep).

**Stage 3a (c5)** — the *previous* session — added the LAPIC-timer-
driven preemption wiring on x86_64:
- A `define_isr!` macro in `kernel/arch/x86_64::interrupts` emits a
  `#[naked]` `extern "C" fn` per vector, sharing the same 15-GPR push
  / 16-byte stack align / call / 15-GPR pop / `iretq` sequence as
  the default thunk in `interrupts.s` but parameterised on the
  dispatcher symbol via `sym`. Hardware-error-code vectors (8,
  10–14, 17, 21) are out of scope.
- A new `kernel/arch/x86_64::preempt` module owns
  `TIMER_VECTOR = 0x20`, a 256-entry `LAPIC_TO_CPU_ID` mapping (LAPIC
  ID → dense `CpuId`), an `AtomicU64`-packed callback storage,
  `rustos_arch_x86_64_timer_dispatch` (the Rust trampoline — reads
  LAPIC ID, calls the registered callback, writes `0` to LAPIC EOI),
  and `unsafe fn init_local_preempt(cpu_index, &mut lapic,
  calibration)` that installs the timer vector into the per-CPU IDT
  (via the new `percpu::install_vector`) and programs the LAPIC
  timer in periodic mode.
- `tests/integration/scheduler_stress_qemu` now calibrates the LAPIC
  timer once on the BSP against PIT channel 2, publishes the
  `Calibration` to APs through a packed `AtomicU64`, installs the
  scheduler-tick callback that forwards into
  `Scheduler::on_timer_tick`, registers LAPIC→CpuId mappings, calls
  `init_local_preempt` on every CPU after `percpu::init`, `sti`s,
  and asserts `preemption_count(cpu) >= 10` per CPU at the end of
  the workload. A silent revert to cooperative-only scheduling
  trips this assertion loudly.
- 4 new host unit tests in `preempt::tests` (vector const, LAPIC
  offsets, callback round-trip, LAPIC→CpuId mapping); 5 new host
  unit tests in `kernel/sched::scheduler::tests` (counter, no
  dispatch from tick, idle-still-counts, error surface, per-CPU
  isolation). Arch-crate host test total = 101. Sched-crate host
  test total = 28 lib + 7+1 integration = 36.

**Stage 2 is still `in progress`** because the Stage-2 deliverable
sub-checklist (`PLAN.md`) requires syscall entry + `KernelArch`
wiring to flip cleanly to `complete`.

## Goal of this session

Deliver the remaining Stage-3a x86_64 items below, then flip Stage 2's
status block to `complete`. Scope is x86_64 only; Stages 3b/3c/3d
remain out of scope.

Concretely you must land, to AGENTS.md quality (full `// SAFETY:`
blocks, tests for every invariant, no `unwrap`/`expect`/`panic!`
outside tests and documented boot invariants, no ambient authority):

1. **(c6) x86_64 syscall entry.**
   `syscall`/`sysret` MSR programming (`IA32_LSTAR`/`IA32_STAR`/
   `IA32_FMASK`), a kernel stack swap via `IA32_KERNEL_GS_BASE`, and a
   thin entry stub that builds the `RawArgs` the architecture-neutral
   `kernel/syscall::Dispatcher` already validates against
   `SYSCALL_TABLE_HASH`. **Do not duplicate any of the syscall-table
   validation surface; reuse it.** Live next to `preempt.rs` in a new
   `kernel/arch/x86_64::syscall_entry` module (or similar — propose a
   name in `PLAN.md` if you want to deviate). Host unit tests must
   cover the `RawArgs` packing logic and the MSR-value math; the
   bare-metal `wrmsr` paths are gated to `target_os = "none"` and
   exercised through the QEMU integration test.

2. **(c7) `kernel/core::KernelArch`.**
   Implement against (c1)..(c6) and wire `kernel_main` so a real
   `kernel_main` can boot to the `init` placeholder. The two QEMU
   integration tests today supply their own `kernel_main`; **do not
   break that contract** — the new wiring lives in a separate path
   the binary opts into (e.g. a `rustos-kernel` bin crate that links
   `kernel/core` + `kernel/arch/x86_64`). Decide commit-split based on
   diff size: if the bin crate plus `KernelArch` impl exceeds ~600
   LOC together, split (c7-impl) and (c7-bin) into two commits.

3. **(d1) Per-arch QEMU runner module.**
   Move x86_64-specific defaults out of the generic `tools/qemu::Spec`
   (RAM size, OVMF flags, `isa-debug-exit` device) into a new
   `tools/qemu/src/x86_64.rs`. The generic `Spec` becomes
   architecture-neutral; per-arch modules own the argv assembly. Add
   unit tests for the new module. The two existing integration tests
   must continue to pass — the refactor is internal.

4. **(d2) Flip Stage 2 status** in `PLAN.md` to `complete` with the
   same evidence style (toolchain, coverage numbers, `cargo xtask ci`
   tail). Tick sub-checklist items 2.1–2.8 and the Stage 3a checklist
   for (c6/c7/d1/d2). If every per-arch checklist item for x86_64 is
   then satisfied, flip Stage 3a to `complete`; otherwise leave Stage
   3a `in progress` with the remaining items honestly enumerated.

## Hard constraints

- Sensible commit split per AGENTS.md §14, each with
  `Co-authored-by: Junie <junie@jetbrains.com>`. (c6) and (c7) are
  typically separate; (d1) and (d2) are one commit each.
- `cargo xtask ci` must be green at HEAD of every commit. Quote the
  tail in the final summary. CI includes the two QEMU integration
  tests via `cargo xtask test --qemu`.
- Coverage floors per `AGENTS.md` §7: `kernel/arch/x86_64` ≥ 85 %;
  `kernel/mem`, `kernel/sec`, `kernel/ipc`, `lib/caps`, `lib/crypto`
  stay ≥ 95 %.
- No `unwrap`/`expect`/`panic!` in production paths. `unsafe` paired
  with `// SAFETY:` and a test or model. No `#[allow(...)]` without a
  justifying comment.
- Docs in the same commit (AGENTS.md §13):
  `docs/src/platform/x86_64.md` gains (c6/c7/d1/d2) sections;
  `docs/src/architecture/{kernel,syscalls}.md` updated to reflect the
  real arch wiring.
- If anything is ambiguous or impossible in one session, **stop and
  ask** (AGENTS.md §15.2 / §15.7) before stubbing. Previous Stage 3a
  sessions honoured this guidance — do the same if the remaining
  surface proves too large for one session.

### Carry-over design notes from (c4)/(c5)

- `kernel/sync::RwLock` is explicitly process-context-only. The
  `Scheduler::on_timer_tick` ISR-safe entry point therefore *only*
  bumps a counter; it does not call `step`. (c6/c7) must respect the
  same rule — any new code that runs in interrupt context must avoid
  the registry `RwLock`, the overflow `SpinLock`, and any other
  process-context-only primitive.
- The `define_isr!` macro is the only sanctioned way to emit a per-
  vector ISR stub on x86_64. Reuse it for the syscall trampoline if
  the dispatch shape fits — but note that `syscall`/`sysret` uses
  `IA32_LSTAR` (a direct MSR-driven entry, *not* an IDT vector), so
  the syscall fast path needs its own naked-fn rather than a
  `define_isr!` invocation. The macro lives so future IDT-driven
  vectors (e.g. a debug exception handler) can re-use it.
- The LAPIC→CpuId mapping in `preempt.rs` is the canonical place to
  look up "which dense CpuId is this CPU?". (c6) may need the same
  mapping for the syscall path — re-use `preempt::cpu_id_for_lapic`
  rather than duplicating the table.

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
  `-smp 4` with real LAPIC-timer-driven preemption (the per-CPU
  preemption-count assertion landed in the previous session is in
  tree and must remain green).
- `cargo xtask ci` green; tail quoted in `PLAN.md` Stage 2 status
  block.
- `PLAN.md` Stage 2 marked `complete`; Stage 3a sub-checklist all
  ticked; Stage 3a marked `complete` if every per-arch checklist
  item for x86_64 is satisfied (otherwise leave Stage 3a
  `in progress` with the remaining items honestly enumerated).
- One commit per logical change with the AGENTS.md §14 trailer.
