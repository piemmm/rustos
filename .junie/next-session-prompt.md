# Continuation Prompt — RustOS Stage 2.7 follow-up (Production syscall wiring)

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

Stages 0..2 are complete. Stage 3a is **complete** for x86_64 —
all (a)..(c7), (d1) items ticked, including the production
`rustos-kernel` bin crate. The (c7-bin) dispatch callback is the
**fail-closed `halt`-on-first-syscall** version (`kernel/rustos-
kernel/src/dispatch.rs::fail_closed_dispatch`). Its `extern "C"`
ABI is pinned at compile time by `_DISPATCH_SIGNATURE_PINNED`.

The earlier session's prompt described the swap to a real
forwarder as a "body-only change". An inspection in the (c7-bin)
follow-up session showed the work is larger: the tree has neither
a production `SyscallHandlers` impl, nor per-CPU current-task
plumbing, nor a kernel-side registration hook on
`kernel_core::kernel_main`. PLAN.md now captures the full
breakdown in the dedicated "Stage 2.7 follow-up — Production
syscall wiring" section as sub-items **(f1)..(f7)**.

## Goal of this session

Land the **Stage 2.7 follow-up** to AGENTS.md quality, item by
item, then tick (f1)..(f7), refresh the Stage 3a status block to
note Stage 2.7 follow-up `complete`, and refresh the Stage 2
evidence tail with a fresh `cargo xtask ci` quote.

The full sub-checklist lives in PLAN.md. Read it first. The short
form is:

- **(f1)** Per-CPU current-task slot on `Scheduler<A>`:
  `current_task(cpu_id) -> Option<TaskId>` updated by `step`,
  cleared on `park` / `exit`. No new `SchedulerArch` method.
- **(f2)** `CapTable` in `kernel/sec::captable` —
  `TaskId -> &TaskCapabilities` registry owned by `KernelState`.
  No global mutable state.
- **(f3)** `KernelSyscallHandlers<'a, A: KernelArch>` in a new
  `kernel/core::syscalls` module wiring `yield_now`, `exit`,
  `cap_query`, `cap_delegate`, `cap_revoke`, `clock_get`.
  `ipc_send` / `ipc_recv` and `cap_delegate`'s `set_ptr` copy-in
  are deferred (no named-port registry; no user-memory copy-in
  yet) — the handlers emit one audit record and return a
  stable `Errno` (`NotFound` / `NotImplemented`) per AGENTS.md
  §15.1. A new `KernelArch::monotonic_ns(cpu_id) -> u64`
  method is added (no default — every arch must opt in).
- **(f4)** Registration hook on `kernel_main`: extend `BootInfo`
  with `dispatcher_callback_slot: &'static DispatchCallbackSlot`
  whose `install_dispatcher` is called between the `Sched` and
  `Ipc` phases. The arch port's `set_dispatch_callback` is still
  invoked before `syscall` is enabled — the new slot is the
  kernel-side publication point, not the trampoline.
- **(f5)** `kernel/rustos-kernel::dispatch` body swap to
  `production_dispatch` that builds a `CallerContext` from
  `current_cpu` → `current_task` → `caps_for`, then forwards to
  `Dispatcher::dispatch`. No-task branch halts the CPU after
  emitting one audit record (AGENTS.md §5.4.5 fail-closed).
- **(f6)** QEMU integration test: a `test-hooks`-gated entry
  point invokes `Dispatcher::dispatch` directly with
  (`cap_query`, `CAP_TIME_SET`) and (`exit`, 0) inside the
  kernel; the audit-observer sink flips `qemu_exit::exit_success`
  on observing both `SyscallInvoked` records. Kernel CPL=0
  cannot legally issue `syscall` itself — fail-closed remains.
- **(f7)** PLAN.md tick + evidence refresh.

## Hard constraints

- One logical change per commit per AGENTS.md §14. Each commit
  carries `Co-authored-by: Junie <junie@jetbrains.com>`. Suggested
  split: one commit per (f1), (f2), (f3), (f4), (f5), (f6), then
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
  `docs/src/architecture/scheduler.md`,
  `docs/src/security/captable.md` (new),
  `docs/src/architecture/syscalls.md`,
  `docs/src/architecture/kernel.md`,
  `docs/src/platform/x86_64.md`,
  `kernel/rustos-kernel/README.md`.
- If anything is ambiguous or impossible in one session, **stop
  and ask** (AGENTS.md §15.2 / §15.7) before stubbing.

### Carry-over design notes (still binding)

- `kernel/sync::RwLock` is process-context-only. The per-CPU
  current-task slot must not be read from interrupt context.
  Syscall entry runs in process context on the issuing CPU.
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

- All (f1)..(f7) implemented to AGENTS.md quality.
- New host unit tests cover every new public API with the
  coverage floors in AGENTS.md §7 (`kernel/sec`,
  `kernel/ipc`, `lib/caps`, `lib/crypto` ≥ 95 %; other kernel
  ≥ 85 %).
- New QEMU integration test boots the `rustos-kernel` binary
  to a `cap_query` + `exit` audit-event pair observed via the
  audit-sink observer.
- All existing QEMU integration tests continue to pass.
- `cargo xtask ci` green; tail quoted in PLAN.md's Stage 2
  status block (or under a new "Stage 2.7 follow-up status"
  block if you choose to record it separately).
- PLAN.md "Stage 2.7 follow-up" sub-checklist (f1)..(f7) all
  ticked; Stage 3a status updated to note the follow-up is
  `complete`.
- One commit per logical change with the AGENTS.md §14 trailer.
