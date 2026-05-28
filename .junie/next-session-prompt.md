# Continuation Prompt — RustOS Stage 3a (c7) + (d1/d2)

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

**Stage 3a (c4/c5)** added the scheduler-side preemption
observation point (`Scheduler::on_timer_tick`,
`Scheduler::preemption_count`, `Scheduler::total_preemption_count`),
the `define_isr!` macro, and the LAPIC-timer-driven preemption
wiring (`kernel/arch/x86_64::preempt`).

**Stage 3a (c6)** — the *previous* session — added x86_64 syscall
entry in a new `kernel/arch/x86_64::syscall_entry` module:

- Pure host-testable MSR-value math: `encode_star(kernel_cs,
  sysret_user_base)`, `efer_with_sce(prev)`, `fmask_value()`
  (`RFLAGS_MASK = 0x7_4700` clears `IF`/`TF`/`DF`/`AC`/`NT`/`RF`/
  `VM`), and `pack_raw_args(rdi,rsi,rdx,r10,r8,r9) ->
  [u64; SYSCALL_MAX_ARGS]` (re-uses `rustos_abi::SYSCALL_MAX_ARGS`;
  the arch crate's only production dep is `rustos-abi`).
- Per-CPU `SyscallTls { kernel_rsp0, user_rsp_save }` block
  (`#[repr(C, align(16))]`, 16 bytes) addressed via
  `IA32_KERNEL_GS_BASE`; static `PER_CPU_TLS: [SyscallTls; MAX_CPUS]`
  arena gated to `target_os = "none"`. `install_kernel_rsp0` writes
  the slot and returns its address; `init_local_syscalls(cpu_index,
  kernel_cs, sysret_user_base, kernel_rsp0)` programs `IA32_EFER.SCE`
  / `IA32_STAR` / `IA32_LSTAR` / `IA32_FMASK` / `IA32_KERNEL_GS_BASE`
  via inline `wrmsr`/`rdmsr`.
- Naked `syscall_entry_stub` (`#[unsafe(naked)]`,
  `target_os = "none"`-gated): `swapgs` → save user `%rsp` to
  `gs:8` / load kernel `%rsp` from `gs:0` → push `%rcx` (user RIP)
  + `%r11` (user RFLAGS) + alignment pad → build the
  `[u64; SYSCALL_MAX_ARGS]` on the kernel stack from
  `%rdi`/`%rsi`/`%rdx`/`%r10`/`%r8`/`%r9` → call Rust trampoline
  `rustos_arch_x86_64_syscall_dispatch(number=rax, args_ptr=rsp)` →
  pop everything → restore user `%rsp` → `swapgs` → `sysretq`.
- Atomic callback storage (`SYSCALL_DISPATCH_CALLBACK: AtomicU64`,
  also `target_os = "none"`-gated, mirroring `preempt`'s
  `TIMER_CALLBACK_FN`) with `set_dispatch_callback(cb)` /
  `dispatch_callback() -> Option<SyscallDispatchFn>`. The
  trampoline fail-closes via `qemu_exit::exit_failure` if it ever
  fires before a callback is installed (same posture as
  `interrupts::rustos_arch_x86_64_default_interrupt`).
- 10 new host unit tests in `syscall_entry::tests` (MSR addresses
  match Intel SDM Vol 4 Table 2-2, STAR-encoding bits + selector
  round-trip, EFER.SCE bit + preservation, FMASK documented-bit
  matrix, RawArgs ordering + width vs. `SYSCALL_MAX_ARGS`,
  `SyscallTls` offsets + size + 16-byte alignment, host-is-None
  callback assertion). Arch-crate host test total: 111.
- Docs added in the same commit: `docs/src/platform/x86_64.md`
  Stage-3a-(c6) section (MSR table, naked-entry sequence,
  `unsafe`-block accounting) and `docs/src/architecture/syscalls.md`
  "Per-architecture entry stubs" table replacing the prior
  out-of-scope bullet for per-arch stubs.

**Stage 2 is still `in progress`** because the Stage-2 deliverable
sub-checklist (`PLAN.md`) requires the `KernelArch` wiring to flip
cleanly to `complete`.

## Goal of this session

Deliver the remaining Stage-3a x86_64 items below, then flip Stage 2's
status block to `complete`. Scope is x86_64 only; Stages 3b/3c/3d
remain out of scope.

Concretely you must land, to AGENTS.md quality (full `// SAFETY:`
blocks, tests for every invariant, no `unwrap`/`expect`/`panic!`
outside tests and documented boot invariants, no ambient authority):

1. **(c7) `kernel/core::KernelArch`.**
   Implement against (c1)..(c6) and wire `kernel_main` so a real
   `kernel_main` can boot to the `init` placeholder. The two QEMU
   integration tests today supply their own `kernel_main`; **do not
   break that contract** — the new wiring lives in a separate path
   the binary opts into (e.g. a `rustos-kernel` bin crate that links
   `kernel/core` + `kernel/arch/x86_64`). The binary is the single
   writer of `syscall_entry::set_dispatch_callback`; the callback
   constructs a `rustos_kernel_syscall::RawArgs` from the
   `[u64; SYSCALL_MAX_ARGS]` the stub builds (the two are
   `#[repr(transparent)]`-compatible) and forwards into
   `Dispatcher::dispatch`. Decide commit-split based on diff size:
   if the bin crate plus `KernelArch` impl exceeds ~600 LOC
   together, split (c7-impl) and (c7-bin) into two commits.

2. **(d1) Per-arch QEMU runner module.**
   Move x86_64-specific defaults out of the generic `tools/qemu::Spec`
   (RAM size, OVMF flags, `isa-debug-exit` device) into a new
   `tools/qemu/src/x86_64.rs`. The generic `Spec` becomes
   architecture-neutral; per-arch modules own the argv assembly. Add
   unit tests for the new module. The two existing integration tests
   must continue to pass — the refactor is internal.

3. **(d2) Flip Stage 2 status** in `PLAN.md` to `complete` with the
   same evidence style (toolchain, coverage numbers, `cargo xtask ci`
   tail). Tick sub-checklist items 2.1–2.8 and the Stage 3a checklist
   for (c6/c7/d1/d2). If every per-arch checklist item for x86_64 is
   then satisfied, flip Stage 3a to `complete`; otherwise leave Stage
   3a `in progress` with the remaining items honestly enumerated.

## Hard constraints

- Sensible commit split per AGENTS.md §14, each with
  `Co-authored-by: Junie <junie@jetbrains.com>`. (c7) is typically
  one or two commits; (d1) and (d2) are one commit each.
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
  `docs/src/platform/x86_64.md` gains (c7/d1/d2) sections;
  `docs/src/architecture/{kernel,syscalls}.md` updated to reflect the
  real arch wiring.
- If anything is ambiguous or impossible in one session, **stop and
  ask** (AGENTS.md §15.2 / §15.7) before stubbing. Previous Stage 3a
  sessions honoured this guidance — do the same if the remaining
  surface proves too large for one session.

### Carry-over design notes from (c4)/(c5)/(c6)

- `kernel/sync::RwLock` is explicitly process-context-only. The
  `Scheduler::on_timer_tick` ISR-safe entry point therefore *only*
  bumps a counter; it does not call `step`. (c7) must respect the
  same rule — any new code that runs in interrupt context must avoid
  the registry `RwLock`, the overflow `SpinLock`, and any other
  process-context-only primitive.
- The `define_isr!` macro is the only sanctioned way to emit a per-
  vector ISR stub on x86_64. (c6) used a dedicated naked-fn instead
  because `syscall`/`sysret` is MSR-driven (`IA32_LSTAR`), *not* an
  IDT vector. The macro lives so future IDT-driven vectors (e.g. a
  debug exception handler) can re-use it.
- The LAPIC→CpuId mapping in `preempt.rs` is the canonical place to
  look up "which dense CpuId is this CPU?". (c7) may need the same
  mapping for the syscall path — re-use `preempt::cpu_id_for_lapic`
  rather than duplicating the table.
- The arch crate has a single production dep (`rustos-abi`). Pulling
  in `kernel/syscall` from arch would invert the layering — the
  dispatcher already depends on `kernel/sec`, `kernel/sched`,
  `lib/log`, and `lib/crypto`. The (c6) callback pattern
  (`SyscallDispatchFn = extern "C" fn(u64, *const [u64;
  SYSCALL_MAX_ARGS]) -> u64`, installed by the binary) is the
  sanctioned bridge; the (c7) `rustos-kernel` binary must wire it
  via `set_dispatch_callback` before calling `init_local_syscalls`.
- Callback installation order matters: the trampoline fail-closes
  via `qemu_exit::exit_failure` if it fires before a callback is
  installed. (c7) must install the dispatch callback *before*
  enabling `syscall` on any CPU.

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
  preemption-count assertion landed in (c5) is in tree and must
  remain green).
- `cargo xtask ci` green; tail quoted in `PLAN.md` Stage 2 status
  block.
- `PLAN.md` Stage 2 marked `complete`; Stage 3a sub-checklist all
  ticked; Stage 3a marked `complete` if every per-arch checklist
  item for x86_64 is satisfied (otherwise leave Stage 3a
  `in progress` with the remaining items honestly enumerated).
- One commit per logical change with the AGENTS.md §14 trailer.
