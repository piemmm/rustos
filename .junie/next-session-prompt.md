# Continuation Prompt — RustOS Stage 2.7 follow-up resume (Production syscall wiring, (f3)..(f7))

Copy the text below verbatim into the next agent session as the
`<issue_description>`.

---

Read `AGENTS.md` and `PLAN.md` in full before doing anything else.
They are binding. Do not skim. In particular: §2 (no hacks, no
duplication, no bloat, no interface creep), §5.4 (the five-step
privileged-entry sequence), §7 (tests must pass; no `#[ignore]`;
coverage floors), §10 (`unsafe` requires `// SAFETY:` + a test +
safe encapsulation), §13 (docs in the same commit), §14 (commit
format, one logical change per commit), §15 (no stubs, no silenced
lints, no weakened security, no invented APIs).

## Context (already on `main`)

Stages 0..2 are complete. Stage 3a is **complete** for x86_64.
The Stage 2.7 follow-up is **partially landed**:

- `c93e823` — kernel/sched: per-CPU current-task slot **(f1)**.
  `Scheduler<A>` exposes `current_task(cpu) -> Option<TaskId>` and
  `yield_current(task_id) -> SchedResult<()>`. Slot is published
  by `dispatch` (set before body, cleared on return) and defensively
  cleared by `park` / `exit` / `yield_current` on matching ids.
  No new `SchedulerArch` method.
- `fcfb5fc` — kernel/sec: `CapTable` registry **(f2)**. A flat
  `BTreeMap<TaskId, TaskCapabilities>` with `insert`, `caps_for`,
  `caps_for_mut`, `remove`, and `len`/`is_empty`. No interior
  mutability — synchronisation policy is the owning scope's
  (`KernelState`) responsibility, to be wired in (f4).

`(c7-bin)` dispatch callback is still the fail-closed
`halt`-on-first-syscall version
(`kernel/rustos-kernel/src/dispatch.rs::fail_closed_dispatch`); its
`extern "C"` ABI is pinned at compile time by
`_DISPATCH_SIGNATURE_PINNED`.

## Goal of this session

Land **(f3) through (f7)** to AGENTS.md quality, item by item, then
tick them in PLAN.md (Stage 2.7 follow-up sub-checklist) and flip
the Stage 2.7 follow-up status block from `partial` to `complete`.
The detailed (f3)..(f7) descriptions live in PLAN.md "Stage 2.7
follow-up" — read them first. The short form is:

- **(f3)** Production `SyscallHandlers` impl in a new
  `kernel/core::syscalls` module: `KernelSyscallHandlers<'a, A>`
  borrowing `&'a Scheduler<A>` and `&'a CapTable`, wiring
  `yield_now` → `Scheduler::yield_current`,
  `exit` → `Scheduler::exit` + `CapTable::remove`,
  `cap_query` → `caps.has(cap)` mapped to `0|1`,
  `cap_delegate` / `cap_revoke` calling into the existing
  `TaskCapabilities::{delegate,revoke}` via `CapTable::caps_for_mut`,
  `clock_get` → new `KernelArch::monotonic_ns(cpu_id) -> u64`.
  `ipc_send` / `ipc_recv` and `cap_delegate`'s `set_ptr` copy-in
  return stable `Errno` (`NotFound` / `NotImplemented`) with one
  audit record each per AGENTS.md §15.1.
  `KernelArch::monotonic_ns` has **no default impl** — every arch
  must opt in; x86_64 wires through `apic_timer::Calibration`.
- **(f4)** Registration hook on `kernel_main`. Extend `BootInfo`
  with `dispatcher_callback_slot: &'static DispatchCallbackSlot`,
  whose `install_dispatcher` is called between the `Sched` phase
  and the `Ipc` phase, after `KernelState` is built. `CapTable`
  is wired into `KernelState` here. The arch port's
  `set_dispatch_callback` is still invoked before `syscall` is
  enabled — the new slot is the kernel-side publication point,
  not the trampoline.
- **(f5)** `kernel/rustos-kernel::dispatch` body swap to
  `production_dispatch` that builds a `CallerContext` from
  `current_cpu` → `current_task` → `caps_for`, then forwards to
  `Dispatcher::dispatch`. No-task branch halts the CPU after
  emitting one audit record (AGENTS.md §5.4.5 fail-closed).
  `_DISPATCH_SIGNATURE_PINNED` stays.
- **(f6)** QEMU integration test: a `test-hooks`-gated entry
  point invokes `Dispatcher::dispatch` directly with
  (`cap_query`, `CAP_TIME_SET`) and (`exit`, 0); the audit-observer
  sink flips `qemu_exit::exit_success` on observing both
  `SyscallInvoked` records.
- **(f7)** PLAN.md tick of (f3)..(f7); flip the Stage 2.7
  follow-up status block to `complete`; refresh the Stage 2
  evidence tail with a fresh `cargo xtask ci` quote.

## Hard constraints

- One logical change per commit per AGENTS.md §14. Each commit
  carries `Co-authored-by: Junie <junie@jetbrains.com>`.
  Suggested split: one commit per (f3), (f4), (f5), (f6), then
  (f7) PLAN.md update.
- `cargo xtask ci` green at HEAD of every commit. Quote the tail
  in the final summary. The new QEMU integration test joins the
  `cargo xtask test --qemu` enrolment list.
- No `unwrap` / `expect` / `panic!` in production paths.
- `unsafe` paired with `// SAFETY:` and a test or model.
- No `#[allow(...)]` without a justifying comment (AGENTS.md
  §15.10).
- Docs land in the same commit as the code they describe
  (AGENTS.md §13). Touched docs include
  `docs/src/architecture/syscalls.md`,
  `docs/src/architecture/kernel.md`,
  `docs/src/platform/x86_64.md`,
  `docs/src/security/captable.md` (gain a "Wiring" / "Lifecycle"
  section that records the (f4) registration step now that the
  registry is composed with `KernelState`),
  `kernel/rustos-kernel/README.md`.
- `rustdoc` on `pub` items must not link to private items
  (`-D rustdoc::private-intra-doc-links` is implied by
  `RUSTDOCFLAGS=-D warnings` in `cargo xtask docs-check`). When a
  public doc comment needs to mention a private method, use
  plain backticks (no `[Self::…]`).
- If anything is ambiguous or impossible in one session, **stop
  and ask** (AGENTS.md §15.2 / §15.7) before stubbing.

### Carry-over design notes (still binding)

- `kernel/sync::RwLock` is process-context-only. `current_task`
  must not be read from interrupt context. Syscall entry runs in
  process context on the issuing CPU.
- `define_isr!` is the only sanctioned ISR stub emitter on
  x86_64. `syscall`/`sysret` is MSR-driven (`IA32_LSTAR`).
- The trampoline fail-closes via `qemu_exit::exit_failure` if it
  fires before a callback is installed. (f5)'s
  `production_dispatch` must be installed via
  `set_dispatch_callback` **before** `syscall` is enabled on any
  CPU — the existing (c7-bin) ordering contract.
- `RawArgs(arr)` is the sanctioned bridge from the kernel-stack
  `[u64; SYSCALL_MAX_ARGS]` to the dispatcher; the compile-time
  `_RAW_ARGS_LAYOUT_MATCHES_ARRAY` assertion locks it.
- `_DISPATCH_SIGNATURE_PINNED` in
  `kernel/rustos-kernel::dispatch` pins the callback ABI; keep
  it green across the swap.
- `CapTable` has no interior mutability. (f4) is the step that
  composes it with `Scheduler` under a single lock-ordering
  policy in `KernelState` (mirror `Scheduler::tasks`'s
  reader-preferring `RwLock` pattern).

## Toolchain & host requirements (already installed on the workbench)

- `nightly-2026-05-27` toolchain (rustc 1.98.0-nightly).
  PATH: `$HOME/.rustup/toolchains/nightly-2026-05-27-x86_64-unknown-linux-gnu/bin`.
- `qemu-system-x86_64` 8.2.2, `grub-mkrescue`, `xorriso`,
  `/usr/share/OVMF/OVMF_CODE_4M.fd` + `OVMF_VARS_4M.fd`,
  `mdbook`, `cargo-deny`, `cargo-llvm-cov` (in `~/.cargo/bin`).
- `mdbook` lives in `~/.cargo/bin`; ensure that directory is on
  `PATH` **before** invoking `cargo xtask ci` — `xtask` does not
  search it automatically.

## Definition of done

- (f3)..(f7) implemented to AGENTS.md quality.
- New host unit tests cover every new public API with the
  coverage floors in AGENTS.md §7 (`kernel/sec`,
  `kernel/ipc`, `lib/caps`, `lib/crypto` ≥ 95 %; other kernel
  ≥ 85 %).
- New QEMU integration test boots the `rustos-kernel` binary to
  a `cap_query` + `exit` audit-event pair observed via the
  audit-sink observer.
- All existing QEMU integration tests continue to pass.
- `cargo xtask ci` green; tail quoted in PLAN.md's Stage 2 status
  block or the Stage 2.7 follow-up status block (now flipping
  from `partial` to `complete`).
- PLAN.md "Stage 2.7 follow-up" sub-checklist (f1)..(f7) all
  ticked; the partial-status block flipped to `complete`.
- One commit per logical change with the AGENTS.md §14 trailer.
