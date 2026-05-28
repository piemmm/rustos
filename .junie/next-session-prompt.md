# Continuation Prompt — RustOS Stage 2.7 follow-up resume (Production syscall wiring, (f4)..(f7))

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

## Context (already on `master`)

Stages 0..2 are complete. Stage 3a is **complete** for x86_64.
The Stage 2.7 follow-up is **partially landed**:

- `c93e823` — kernel/sched: per-CPU current-task slot **(f1)**.
- `fcfb5fc` — kernel/sec: `CapTable` registry **(f2)**.
- `4497106` — kernel/core: production `SyscallHandlers` impl **(f3)**.
  New module `kernel/core::syscalls` ships
  `KernelSyscallHandlers<'a, A: KernelArch>` borrowing
  `&'a Scheduler<A>`, `&'a RwLock<CapTable>`, `&'a A`, and
  `&'a (dyn Sink + Sync)`. Every handler is wired:
  `yield_now → Scheduler::yield_current`,
  `exit → CapTable::remove + Scheduler::exit`,
  `cap_query → caller.caps.has(cap)` mapped to `0|1`,
  `cap_revoke → CapTable::caps_for_mut(target).revoke(cap, audit)`,
  `clock_get → KernelArch::monotonic_ns(arch.current_cpu())`. The
  deferred branches return stable errnos plus a new
  `AuditEvent::SyscallFeatureUnavailable` (id 4020) record:
  `ipc_send`/`ipc_recv` → `NotFound` (named-port registry not
  landed); `cap_delegate` → `NotImplemented` (user-memory copy-in
  not landed). `KernelArch::monotonic_ns(cpu) -> u64` is a new
  trait method with **no default impl**; the x86_64 port wires it
  through `apic_timer::Calibration::tsc_per_second` (sampled across
  the same PIT calibration window via the new `TscReader`/`Rdtsc`
  injection) and a saturating `Calibration::tsc_ticks_to_ns`
  helper. `lib/abi::Errno` appended `NotImplemented = 12`
  (`SYSCALL_TABLE_HASH` is unaffected — the encoded table covers
  syscall specs, not `Errno` variants). `cargo xtask ci` green at
  HEAD.

`(c7-bin)` dispatch callback is **still** the fail-closed
`halt`-on-first-syscall version
(`kernel/rustos-kernel/src/dispatch.rs::fail_closed_dispatch`); its
`extern "C"` ABI is pinned at compile time by
`_DISPATCH_SIGNATURE_PINNED`.

## Goal of this session

Land **(f4) through (f7)** to AGENTS.md quality, item by item, then
tick them in PLAN.md (Stage 2.7 follow-up sub-checklist) and flip
the Stage 2.7 follow-up status block from `partial` to `complete`.
The detailed (f4)..(f7) descriptions live in PLAN.md "Stage 2.7
follow-up" — read them first. The short form is:

- **(f4)** Registration hook on `kernel_main`. Extend `BootInfo`
  with `dispatcher_callback_slot: &'static DispatchCallbackSlot`,
  whose `install_dispatcher` is called between the `Sched` phase
  and the `Ipc` phase, after `KernelState` is built. `CapTable` is
  wired into `KernelState` here under the same reader-preferring
  `RwLock` pattern `Scheduler::tasks` already uses (mirror the
  carry-over note in (f2)). The arch port's
  `set_dispatch_callback` is still invoked before `syscall` is
  enabled — the new slot is the *kernel-side publication* point,
  not the trampoline. No global mutable static; the `&'static`
  reference is to memory the bin crate's `#[link_section]`
  reserves at compile time. Tests:
  `kernel/core/tests/kernel_main.rs` gains a registration-ordering
  test that fails if `BootCompleted` fires without
  `install_dispatcher` being called. Docs:
  `docs/src/architecture/kernel.md` "Syscall registration phase" +
  `docs/src/security/captable.md` "Wiring / Lifecycle" section.

- **(f5)** `kernel/rustos-kernel::dispatch` body swap. Replace
  `fail_closed_dispatch` with `production_dispatch` that builds a
  `CallerContext` from `current_cpu` → `current_task` →
  `caps_for`, then forwards to `Dispatcher::dispatch`. If
  `current_task` returns `None` (no task running on this CPU —
  should be impossible once the scheduler is live but AGENTS.md
  §5.4.5 *fail closed*), the callback emits one
  `SyscallHandlerRejected`-equivalent audit record and halts the
  CPU exactly as the fail-closed version does. Compile-time
  `_DISPATCH_SIGNATURE_PINNED` stays. New host unit tests cover
  the no-task and happy-path branches via the `extern "C"` shim
  already in `dispatch.rs::tests`. Docs:
  `kernel/rustos-kernel/README.md` "Production dispatch callback"
  + `docs/src/platform/x86_64.md` "(c7-bin) Stage 2.7 follow-up"
  tail.

- **(f6)** QEMU integration test. A `test-hooks`-gated entry point
  invokes `Dispatcher::dispatch` directly with
  (`cap_query`, `CAP_TIME_SET`) and (`exit`, 0); the
  audit-observer sink flips `qemu_exit::exit_success` on observing
  both `SyscallInvoked` records. The hook is gated off by default
  and `cargo deny check` rejects accidental release builds that
  enable it. Joins the `cargo xtask test --qemu` enrolment list.

- **(f7)** PLAN.md tick of (f3)..(f6) (note: (f3) was landed in a
  previous session, **but its checkbox has not been ticked yet** —
  the (f1)+(f2) tick commit predates (f3)); add the entry for
  (f7) itself; flip the Stage 2.7 follow-up status block to
  `complete`; refresh the Stage 2 evidence tail with a fresh
  `cargo xtask ci` quote.

## Hard constraints

- One logical change per commit per AGENTS.md §14. Each commit
  carries `Co-authored-by: Junie <junie@jetbrains.com>`.
  Suggested split: one commit per (f4), (f5), (f6), then (f7)
  PLAN.md update.
- `cargo xtask ci` green at HEAD of every commit. Quote the tail
  in the final summary.
- No `unwrap` / `expect` / `panic!` in production paths.
- `unsafe` paired with `// SAFETY:` and a test or model.
- No `#[allow(...)]` without a justifying comment (AGENTS.md
  §15.10).
- Docs land in the same commit as the code they describe
  (AGENTS.md §13).
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
- `CapTable` has no interior mutability of its own. (f3) wraps it
  in `kernel/sync::RwLock` at the *handler* layer; (f4) is the
  step that composes that lock into `KernelState`'s
  lock-ordering policy alongside `Scheduler::tasks`.
- `KernelSyscallHandlers::new(sched, caps, arch, audit)` is the
  one entry point into the (f3) wiring. `KernelState` constructs
  one and hands its `&dyn SyscallHandlers` to a `Dispatcher`
  cell published through the new `DispatchCallbackSlot`.
- `KernelArch::monotonic_ns(cpu) -> u64` is **required** on every
  arch port. aarch64/riscv64/wasm32 ports added later must opt
  in (CNTVCT_EL0 / `rdtime` / `performance.now()` are the
  natural sources); the x86_64 wiring (`apic_timer::Calibration`
  + RDTSC) is the reference.

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

- (f4)..(f7) implemented to AGENTS.md quality.
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
