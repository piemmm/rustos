# Continuation Prompt — RustOS Stage 2.7 follow-up resume ((f6) and (f7))

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
Stage 2.7 follow-up sub-items (f1)..(f5) are landed:

- `c93e823` — kernel/sched: per-CPU current-task slot **(f1)**.
- `fcfb5fc` — kernel/sec: `CapTable` registry **(f2)**.
- `4497106` — kernel/core: production `SyscallHandlers` impl **(f3)**
  (`KernelSyscallHandlers<'a, A>` + `KernelArch::monotonic_ns`).
- `eca9e89` — kernel/core: `DispatchCallbackSlot`, `Phase::Syscall`,
  `KernelDispatchHook`, `KernelState` wiring **(f4)**. `BootInfo`
  gained a `dispatcher_callback_slot: &'static DispatchCallbackSlot`
  field; `kernel_main` `Box::leak`s `KernelState` and publishes a
  `KernelDispatchHook` through the slot between Sched and Ipc.
  `InitError::DispatcherAlreadyInstalled` fail-closes a double
  publish under `phase = "syscall"`.
- `45c21c3` — kernel/rustos-kernel: `production_dispatch` swap **(f5)**.
  `boot.rs` installs `production_dispatch` (not `fail_closed_dispatch`).
  `production_dispatch` reads `DISPATCH_SLOT.get()` and forwards through
  the hook; the Errno encoder (`encode_result`) negates the
  discriminant for the userland convention. Halts the CPU on empty
  slot or `DispatchOutcome::NoCallerContext`.

`cargo xtask ci` is green at HEAD (`45c21c3`).

## Goal of this session

Land **(f6) and (f7)** to AGENTS.md quality, then flip the Stage 2.7
follow-up status block from `partial` to `complete`. The detailed
descriptions live in PLAN.md "Stage 2.7 follow-up" — read them first.

- **(f6)** QEMU integration test. A `test-hooks`-gated entry point
  drives `Dispatcher::dispatch` directly with
  (`cap_query`, `CAP_TIME_SET`) and (`exit`, 0); an audit-observer
  sink flips `qemu_exit::exit_success` on observing both
  `SyscallInvoked` records. The hook is gated off by default and
  release builds that enable it must be rejected (the prompt's
  cargo-deny check; the cleanest implementation is likely a
  `compile_error!` guard plus a deny.toml `bans` rule). Joins the
  `cargo xtask test --qemu` enrolment list.

  *Design hint:* directly bypassing `DISPATCH_SLOT.get()`'s
  `Scheduler::current_task` is the cleanest path — synthesise a
  `Scheduler`/`CapTable`/`KernelSyscallHandlers`/`Dispatcher` quartet
  in the test bin's audit-observer sink and call `dispatcher.dispatch`
  on the `BootCompleted` event before `qemu_exit::exit_success`.
  Going through the production hook would also require registering
  a real task and driving the scheduler, which the prompt explicitly
  marks out of scope for this follow-up.

- **(f7)** PLAN.md (f6) tick; add the (f7) commit-id line itself;
  flip the Stage 2.7 follow-up status block to `complete`; refresh
  the Stage 2 evidence tail with a fresh `cargo xtask ci` quote.

## Hard constraints

- One logical change per commit per AGENTS.md §14. Each commit
  carries `Co-authored-by: Junie <junie@jetbrains.com>`.
- `cargo xtask ci` green at HEAD of every commit. Quote the tail in
  the final summary.
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
  fires before a callback is installed. `production_dispatch`
  remains installed via `set_dispatch_callback` **before**
  `syscall` is enabled on any CPU — the existing (c7-bin)
  ordering contract.
- `DISPATCH_SLOT` is a `pub static DispatchCallbackSlot` in
  `kernel/rustos-kernel/src/dispatch.rs`. The set-once publish is
  protected by `kernel/sync::OnceCell`; no global mutable static.
- `RawArgs(arr)` is the sanctioned bridge from the kernel-stack
  `[u64; SYSCALL_MAX_ARGS]` to the dispatcher; the compile-time
  `_RAW_ARGS_LAYOUT_MATCHES_ARRAY` assertion locks it.
- `_DISPATCH_SIGNATURE_PINNED` in
  `kernel/rustos-kernel::dispatch` pins the callback ABI.
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

- (f6)..(f7) implemented to AGENTS.md quality.
- New host unit tests cover every new public API with the
  coverage floors in AGENTS.md §7.
- New QEMU integration test boots the `rustos-kernel` binary
  (built with the `test-hooks` feature) to a `cap_query` + `exit`
  audit-event pair observed via the audit-sink observer.
- All existing QEMU integration tests continue to pass.
- `cargo xtask ci` green; tail quoted in PLAN.md's Stage 2.7
  follow-up status block (now flipping from `partial` to
  `complete`).
- PLAN.md "Stage 2.7 follow-up" sub-checklist (f1)..(f7) all
  ticked; the partial-status block flipped to `complete`.
- One commit per logical change with the AGENTS.md §14 trailer.
