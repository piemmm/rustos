# Continuation Prompt — RustOS Stage 3a (c7)

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

Stages 0–1 are complete. **Stage 2 is now `complete`** — the
`scheduler_stress` deliverable text under QEMU on ≥ 4 cores was
satisfied by Stage 3a (b) (AP startup, APIC bring-up, INIT-SIPI-SIPI)
and Stage 3a (c5) (real LAPIC-timer-driven preemption with a
`preemption_count(cpu) >= 10`-per-CPU assertion).

**Stage 3a is `in progress`**, with all per-arch checklist items
ticked **except (c7)**:

- **(a)** UEFI / Multiboot2 memory-map hand-off, ACPI MADT parse,
  APIC bring-up + LAPIC-timer calibration — `kernel/arch/x86_64`
  modules `multiboot2`, `acpi`, `apic`, `apic_timer`, `bootmemory`.
- **(b)** AP startup via INIT-SIPI-SIPI trampoline at `0x8000`
  (`smp::TrampolineFrame` / `ApBootSlot` / `init_sipi_sipi`); identity
  map widened to 0..4 GiB; `kernel_main(multiboot_info: u64)` symbol;
  `tests/integration/scheduler_stress_qemu` (cooperative, `-smp 4`).
- **(c1/c2/c3)** Per-CPU GDT + TSS + IST primitives, `TaskCtx` context
  switch, common fail-closed ISR prologue, per-CPU GDT/IDT bring-up via
  `percpu::init`.
- **(c4)** Scheduler preemption observation point
  (`Scheduler::on_timer_tick`, `preemption_count`,
  `total_preemption_count`) — ISR-safe, counter-only.
- **(c5)** LAPIC-timer-driven preemption: `define_isr!` macro,
  `kernel/arch/x86_64::preempt` (`TIMER_VECTOR = 0x20`,
  `LAPIC_TO_CPU_ID`, fail-closed dispatcher,
  `init_local_preempt(cpu_index, &mut lapic, calibration)`).
  `scheduler_stress_qemu` now asserts `preemption_count(cpu) >= 10` per
  CPU.
- **(c6)** x86_64 syscall entry stub in
  `kernel/arch/x86_64::syscall_entry`:
  - host-testable MSR-value math (`encode_star`, `efer_with_sce`,
    `fmask_value`, `pack_raw_args`),
  - per-CPU `SyscallTls { kernel_rsp0, user_rsp_save }` block
    addressed via `IA32_KERNEL_GS_BASE`,
  - naked `syscall_entry_stub` (`swapgs` → save `%rsp` → load kernel
    `%rsp` → push `%rcx`/`%r11` → build the
    `[u64; SYSCALL_MAX_ARGS]` frame → call Rust trampoline → pop →
    `swapgs` → `sysretq`),
  - `SYSCALL_DISPATCH_CALLBACK: AtomicU64` with
    `set_dispatch_callback(cb)` / `dispatch_callback()`; the
    trampoline fail-closes via `qemu_exit::exit_failure` if it ever
    fires before a callback is installed (mirroring
    `interrupts::rustos_arch_x86_64_default_interrupt` and
    `preempt::TIMER_CALLBACK_FN`).
  - 10 host unit tests cover MSR addresses, STAR encoding, EFER.SCE,
    FMASK, `RawArgs` ordering, and `SyscallTls` layout. Arch-crate
    host total: 110.
- **(d1)** Per-arch QEMU runner module
  (`tools/qemu/src/x86_64.rs`): `DEFAULT_RAM_MIB`,
  `ISA_DEBUG_EXIT_IOPORT/IOSIZE`, `QEMU_BINARY`, OVMF/UEFI argv
  assembly, GRUB-EFI ISO build dispatch. `Spec`/`Runner` are now
  architecture-neutral; `Runner::run` matches on `Spec::arch` into the
  per-arch backend. The two existing integration tests
  (`memory_isolation`, `scheduler_stress_qemu`) and the
  `cargo xtask test --qemu` driver are untouched. 18 host unit tests
  in `rustos-qemu`.
- **(d2)** PLAN.md: Stage 2 flipped `complete`; Stage 3a sub-checklist
  ticked for (c6) and (d1); `docs/src/platform/x86_64.md` gained (d1)
  and (d2) sections.

## Goal of this session

Land **(c7)** to AGENTS.md quality, then flip Stage 3a to `complete`
in PLAN.md (or leave `in progress` if any honest gap remains —
**no stubs, no `todo!()`, no `#[allow]` without justification**).

Concretely you must land:

1. **`SchedulerArch` + `KernelArch` impl for x86_64.**
   The trait surface is `kernel/sched::SchedulerArch::current_cpu` +
   `send_ipi` and `kernel/core::KernelArch::halt`. `current_cpu` re-uses
   `preempt::cpu_id_for_lapic` (which is the canonical LAPIC→CpuId
   mapping). `send_ipi` issues a directed IPI through the BSP-side
   `Lapic` API. `halt` masks interrupts and loops on `hlt`.

2. **`rustos-kernel` bin crate** that links `kernel/core` +
   `kernel/arch/x86_64` + `kernel/syscall` and is the *single writer*
   of `syscall_entry::set_dispatch_callback`. The callback constructs a
   `rustos_kernel_syscall::RawArgs` from the
   `[u64; SYSCALL_MAX_ARGS]` the naked stub builds; **first job of the
   session** is to verify whether `RawArgs` is currently
   `#[repr(transparent)]` over `[u64; N]` and, if not, decide between
   (a) making it so (with a `const _: ()` size+align+layout assert) or
   (b) bridging via an explicit `RawArgs::from_array` constructor. Pick
   the one that does not invent a new public surface (AGENTS.md §2.4).

3. **`kernel_main(multiboot_info: u64)`** body that:
   - validates multiboot magic via the existing `entry.rs` shim,
   - parses Multiboot2 → ACPI RSDP → MADT → `BootMemoryMap` via the
     Stage 3a (a) primitives,
   - builds an `IdentityTableBuilder` + `SchedulerConfig`,
   - constructs `BootInfo` and forwards to
     `rustos_kernel_core::kernel_main`,
   - installs the dispatch callback **before** enabling `syscall` on
     any CPU,
   - wires `percpu::init`, `preempt::init_local_preempt`, and
     `syscall_entry::init_local_syscalls` per CPU.

4. **Panic-handler bridge.** The `rustos-kernel` bin's
   `#[panic_handler]` delegates to `kernel_core::handle_panic` with a
   `PanicContext` built from the arch crate's halt path.

5. **Tests + docs in the same commit.** New host unit tests for the
   `SchedulerArch` + `KernelArch` impls (mock `Lapic` + assertion that
   `halt` is `-> !`). A new QEMU integration test
   (`tests/integration/kernel_arch_boot` or similar) that builds the
   `rustos-kernel` binary and asserts boot reaches the
   `BootCompleted` audit record. Coverage floors per `AGENTS.md` §7:
   `kernel/arch/x86_64` ≥ 85 %; `kernel/mem`, `kernel/sec`,
   `kernel/ipc`, `lib/caps`, `lib/crypto` stay ≥ 95 %.

6. **PLAN.md update.** Tick (c7); flip Stage 3a to `complete`. Refresh
   the Stage 2 evidence tail with a fresh `cargo xtask ci` quote.

## Hard constraints

- Sensible commit split per AGENTS.md §14, each with
  `Co-authored-by: Junie <junie@jetbrains.com>`. If the bin crate +
  `KernelArch` impl together exceed ~600 LOC, split (c7-impl) and
  (c7-bin) into two commits.
- `cargo xtask ci` must be green at HEAD of every commit. Quote the
  tail in the final summary. CI includes the QEMU integration tests
  via `cargo xtask test --qemu` — the new boot-to-init test joins
  that list.
- No `unwrap`/`expect`/`panic!` in production paths. `unsafe` paired
  with `// SAFETY:` and a test or model. No `#[allow(...)]` without a
  justifying comment.
- Docs in the same commit (AGENTS.md §13):
  `docs/src/platform/x86_64.md` gains a (c7) section;
  `docs/src/architecture/{kernel,syscalls}.md` updated to reflect the
  real arch wiring.
- If anything is ambiguous or impossible in one session, **stop and
  ask** (AGENTS.md §15.2 / §15.7) before stubbing.

### Carry-over design notes

- `kernel/sync::RwLock` is explicitly process-context-only. Anything
  that runs in interrupt context must avoid the registry `RwLock`,
  the overflow `SpinLock`, and any other process-context-only
  primitive. (c4) installed `Scheduler::on_timer_tick` as the only
  ISR-safe writer.
- The `define_isr!` macro is the only sanctioned way to emit a
  per-vector ISR stub on x86_64. `syscall`/`sysret` is MSR-driven
  (`IA32_LSTAR`), not an IDT vector; (c6)'s dedicated naked-fn is the
  only exception.
- The arch crate's single production dep is `rustos-abi`. (c7) is the
  first commit that pulls `kernel/core` into the kernel binary; the
  arch crate itself should stay light. The dispatcher already depends
  on `kernel/sec`, `kernel/sched`, `lib/log`, and `lib/crypto`. The
  (c6) callback pattern (`SyscallDispatchFn = extern "C" fn(u64,
  *const [u64; SYSCALL_MAX_ARGS]) -> u64`, installed by the binary)
  is the sanctioned bridge.
- Callback installation order matters: the trampoline fail-closes via
  `qemu_exit::exit_failure` if it fires before a callback is
  installed. The (c7) bin crate must install the dispatch callback
  *before* enabling `syscall` on any CPU.

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

- (c7) implemented to AGENTS.md quality.
- A new QEMU integration test boots the `rustos-kernel` binary all
  the way to `kernel_core::AuditEvent::BootCompleted`, with the
  scheduler-tick callback wired and the syscall dispatch callback
  installed before `syscall` is enabled.
- All existing QEMU integration tests continue to pass.
- `cargo xtask ci` green; tail quoted in `PLAN.md` Stage 2 status
  block (or a new Stage 3a status block if you choose to record it
  separately).
- `PLAN.md` Stage 3a sub-checklist (c7) ticked; Stage 3a flipped to
  `complete` if no other x86_64 gap remains.
- One commit per logical change with the AGENTS.md §14 trailer.
