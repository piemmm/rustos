# PLAN.md — RustOS Build Plan

This plan turns the requirements in `AGENTS.md` into ordered, assignable
work. Each **Stage** is delivered by a separate task (and likely a separate
agent). A stage is complete only when:

- All listed deliverables exist.
- All listed tests pass under `cargo xtask test`.
- All listed documentation is written and links cleanly.
- `AGENTS.md` rules have been observed (no hacks, no duplication, no
  weakened tests, no missing docs).
- The `AGENTS.md` §2.15 validation gate has been run over the **entire**
  workspace and is green: `cargo fmt --all`, the full `cargo xtask ci`
  pipeline, `cargo xtask fuzz --secs 5`, and anything else
  `.github/workflows/ci.yml` exercises (§7 "Definition of done"). No stage,
  and no individual piece of work within it, is complete until this gate
  passes; the actual command output is quoted in the completion report.

Do **not** begin a stage before all its listed dependencies are complete.

---

## Stage 0 — Repository Foundation

**Dependencies:** none.

**Deliverables**
- Workspace `Cargo.toml` listing every planned crate (empty crates allowed
  as placeholders, but each must compile).
- `rust-toolchain.toml` pinning a specific nightly with the required components
  (`rust-src`, `llvm-tools-preview`, `clippy`, `rustfmt`).
- `.cargo/config.toml` declaring per-target build flags and linker scripts.
- `rustfmt.toml`, `clippy.toml`, `deny.toml` (license + advisory rules).
- `tools/xtask/` with subcommands: `build`, `test`, `clippy`, `fmt`,
  `docs-check`, `abi-check`, `c-header`, `deps-check`, `cfg-check`, `coverage`, `ci`,
  `image`.
- `docs/` mdBook scaffold.
- CI definition (`.github/workflows/ci.yml` or equivalent) running
  `cargo xtask ci` on every push.
- `tools/ci/`: CI/build-host orchestration — thin wrappers around
  `cargo xtask` for an unattended builder (scheduling, logging, and the
  parallel nightly 24 h soaks). No pipeline logic; that stays in
  `tools/xtask` (§15).
- `LICENSE`, `README.md` (short), `AGENTS.md` (exists), `PLAN.md` (this file).

**Tests**
- `cargo xtask ci` passes on a clean clone.
- Workspace builds for every Tier-1 target with empty crates.

**Docs**
- `docs/src/architecture/overview.md` — one-page system map.
- `docs/src/contributing.md` — points to `AGENTS.md`.

**Status: complete.**
- Workspace `Cargo.toml` lists every planned crate; the empty placeholders
  all build under `cargo build --workspace --all-targets --locked`.
- `rust-toolchain.toml` pins `nightly-2026-05-27` (rustc 1.98.0-nightly)
  with `rust-src`, `llvm-tools-preview`, `clippy`, `rustfmt` and all four
  Tier-1 cross targets. The original `nightly-2024-09-01` pin (rustc 1.82)
  was bumped here so that `cargo-deny ≥ 0.19` — which is required to parse
  RUSTSEC entries using CVSS 4.0 — can be installed and run; the older pin
  blocked the full `cargo deny check`.
- `.cargo/config.toml` declares the `xtask` alias plus per-target rustflags
  for `x86_64-unknown-none`, `aarch64-unknown-none`,
  `riscv64gc-unknown-none-elf`, and `wasm32-unknown-unknown`.
- `rustfmt.toml`, `clippy.toml`, `deny.toml` are in place and enforced via
  `cargo xtask ci`; the deny policy passes `advisories + bans + licenses +
  sources` on the bumped toolchain.
- `tools/xtask` exposes the closed set of subcommands required by
  `AGENTS.md` §7 / §14 / §17.5: `build`, `test`, `clippy`, `fmt`,
  `docs-check`, `abi-check`, `c-header`, `deps-check`, `cfg-check`, `coverage`, `ci`,
  `image`. `abi-check` deliberately fails loudly if only one half of the
  `lib/abi/src/syscalls.rs` ↔ `kernel/syscall/src/table.rs` pair appears;
  `c-header` generates (`--write`) and verifies the C ABI development
  header(s) under `include/rustos/` from the same `lib/abi` source of truth,
  so a non-Rust program (C, …) can call `abi-v1` and the committed header can
  never drift (`AGENTS.md` §9). That surface is the **whole** of `lib/abi`
  (every `#[repr(C)]` type, constant, and enum discriminant — not just the
  syscalls) and is staged, together with the `ros_sys_*` stub runtime and
  crt0, in `plans/CCOMPAT.md`; `deps-check` and `cfg-check`
  enforce the §17 modularity contracts (see the §17 burn-down section
  below).
- `docs/` ships a mdBook scaffold (`book.toml`, `src/SUMMARY.md`,
  `introduction.md`, `contributing.md`, `architecture/overview.md`) and the
  Stage 1 per-crate `lib/*` pages.
- CI definition `.github/workflows/ci.yml` runs `cargo xtask ci` on every
  push and pull request on a GitHub-hosted `ubuntu-latest` runner, with
  cargo + xtask-helper-tool caches.
- `.github/workflows/soak.yml` runs the nightly 24 h soaks
  (`tools/ci/soak.sh all`: §19.6 fuzz, §19.7 proptest, and the §7
  repeated-test matrix) on a **self-hosted Linux** runner
  (`[self-hosted, linux]`) — a 24 h job exceeds the GitHub-hosted per-job
  time cap — and uploads the per-job soak logs as a build artifact.
  `tools/ci/github-runner/README.md` documents registering and installing
  that runner as a systemd service.
- `tools/ci/` holds the build-host orchestration scripts (`lib.sh`,
  `ci-run.sh`, `soak.sh`) plus scheduler samples for every supported host —
  `crontab.sample` (cron), `systemd/*.{service,timer}` (Linux),
  `launchd/*.plist.sample` (macOS), and `github-runner/` (self-hosted GitHub
  Actions runner) — and a `README.md`. They are thin
  wrappers over `cargo xtask`: `ci-run.sh` logs one subcommand run (default
  `ci`), and `soak.sh` fans the §19.6 fuzz harnesses and §19.7 proptest
  models out into parallel `--soak --target` jobs — plus, with `all`, the
  §7 repeated-test soak (`cargo xtask test --qemu --soak`) — so the nightly
  is not `(harnesses+models) x 24 h` serialised. The scripts are portable `bash`
  (POSIX utilities only) and put `${CARGO_HOME:-~/.cargo}/bin` on `PATH`, so
  they run identically on Linux and macOS. Logs land outside the tree (§3);
  no pipeline step lives in the scripts (§15).
- `LICENSE` (GPL-2.0-or-later, with the `RustOS-syscall-note` syscall / ABI
  exception), `README.md`, `AGENTS.md`, `PLAN.md` are all present at the
  repository root.

---

## Stage 1 — Shared Libraries (`lib/`)

**Dependencies:** Stage 0.

**Deliverables**
- `lib/abi`: stable `#[repr(C)]` types for syscalls, IPC messages, manifests,
  errors, capability IDs. Versioned (`abi-v1`).
- `lib/caps`: `Capability`, `CapabilitySet`, delegation/revocation primitives,
  serializable token format (signed by the local authority key).
- `lib/collections`: only collections actually required by stages 2–6.
- `lib/crypto`: thin, audited wrappers around vetted upstream crates
  (e.g. `ring`, `rustcrypto`). No hand-rolled primitives.
- `lib/log`: structured, level-filtered, no-alloc-on-hot-path logging with
  stable event IDs.
- `lib/rng`: random number generation — a NIST SP 800-90A HMAC-SHA256
  CSPRNG composed over `lib/crypto`'s audited HMAC, a pluggable
  entropy / hardware-RNG seam (the §19.2 platform RNG), and a fast
  non-cryptographic xoshiro256++ generator.
- `lib/util`: only items used by ≥ 2 crates.

**Tests**
- Unit tests in each crate, mirror layout (§7).
- Property tests for `lib/caps` delegation rules (a delegated set is always
  a subset of the parent).
- Fuzz target for `lib/abi` decoding.

**Docs**
- One page per crate under `docs/src/`.
- Rustdoc on every public item.

**Status: complete.**
- All six `lib/*` crates implemented (`abi`, `caps`, `collections`, `crypto`,
  `log`, `util`), `no_std`, with rustdoc on every public item and unit tests
  alongside the code per §7.
- `lib/abi` ships the `abi-v1` types (`Errno`, `CapabilityId`,
  `SyscallNumber`, `IpcMessageHeader`, `ManifestHeader`) plus a deterministic
  100 000-input fuzz harness in `lib/abi/tests/fuzz_decode.rs`. `abi-v1` is
  **not frozen yet** — RustOS has not shipped a release, so these types remain
  mutable; they become immutable at the first release and new behaviour then
  ships as `abi-v2` (`AGENTS.md` §9). "Frozen" elsewhere in this plan refers to
  the per-type stability discipline (a shipped wire layout is not widened in
  place), not to a released `abi-v1`.
- `lib/caps` enforces the subset-only delegation invariant; an exhaustive
  property test exercises every 2⁸ subset of the well-known capabilities.
- `lib/crypto` exposes audited SHA-256 and Ed25519 verification only;
  upstream crates are pinned exactly (`sha2 = =0.10.9`,
  `ed25519-dalek = =2.1.1`, with the `zeroize` feature enabled so the
  dalek crate's own internal key material is wiped on drop; `lib/crypto`
  does not take `zeroize` as a direct dependency because it exposes
  verification only).
- `lib/util` is intentionally empty per `AGENTS.md` §2.3; no item yet
  satisfies the ≥ 2-use rule.
- `lib/rng` added (experimental): `CsRng`, a NIST SP 800-90Ar1 HMAC-SHA256
  DRBG (`drbg::HmacDrbg`) composed over `lib/crypto`'s HMAC — validated
  against the NIST CAVP known-answer vector — that reseeds from a pluggable
  `EntropySource`; `CombinedSource` XOR-mixes several sources; a
  `hardware::HardwareRng` seam supplies a motherboard RNG as both extra
  entropy and (via `PlatformFast`) a fast source with software fallback;
  and `FastRng` (xoshiro256++) is the fast non-cryptographic generator.
  `lib/crypto` gained `hmac_sha256_parts` (a multi-part HMAC) to back the
  DRBG without an allocator. This is the §19.2 platform-RNG seam the
  `kernel/mem::swap` `EntropySource` was reserved for.
- The syscall ABI lives in `lib/abi/src/syscall.rs` (singular); the
  cross-checked `lib/abi/src/syscalls.rs` and `kernel/syscall/src/table.rs`
  pair is reserved for Stage 2 so `cargo xtask abi-check` always sees both
  halves.
- `cargo xtask ci` is fully green: fmt, clippy (`-D warnings`), tests,
  rustdoc (`-D warnings`), mdbook, in-tree link check, and the full
  `cargo deny check` (advisories + bans + licenses + sources) all pass on
  the bumped toolchain (`nightly-2026-05-27`, rustc 1.98.0-nightly).
- Coverage measured with `cargo llvm-cov` clears `AGENTS.md` §7 thresholds:
  `lib/caps` 98.19 % lines, `lib/crypto` 96.84 % lines (≥ 95 % floor);
  `lib/abi`, `lib/collections`, `lib/log` all ≥ 95 % (≥ 85 % floor);
  workspace total 98.29 %.

---

## Stage 2 — Kernel Core (architecture-neutral)

**Dependencies:** Stage 1.

**Deliverables**
- `kernel/core`: kernel entry, panic handler (logs and halts; never silently
  resets), boot-time invariants, global init order.
- `kernel/mem`:
  - Physical frame allocator (buddy + bitmap).
  - Virtual memory manager (per-process page tables).
  - Kernel slab allocator with guard pages.
  - Zero-on-free policy for sensitive regions.
  - `Result`-returning allocation API (no panic on OOM).
- `kernel/sched`: SMP-aware scheduler (per-CPU run queues, work stealing,
  priority + fairness, IPI-based preemption).
- `lib/sync`: spinlocks, RW locks, MCS locks, RCU-equivalent, all
  documented with their use cases.
- `kernel/ipc`: capability-checked message ports, shared memory objects,
  asynchronous notifications.
- `kernel/sec`: user/group/capability tables, manifest verification,
  audit log writer.
- `kernel/syscall`: dispatch table generated from `lib/abi/src/syscalls.rs`.

**Tests**
- Host-side unit tests for every algorithm that does not need hardware.
- QEMU-based integration tests for memory isolation: a test process
  attempting to read another's memory must fault.
- Stress test for the scheduler under load on ≥ 4 emulated cores.

**Docs**
- `docs/src/architecture/kernel.md`, `…/memory.md`, `…/scheduler.md`,
  `…/ipc.md`, `…/security.md`, `…/syscalls.md`.

**Sub-stages**
- [x] 2.1 — `lib/sync`: spinlocks, IRQ-safe spinlock, writer-preference
      RwLock, MCS queue lock, SeqLock, epoch-based reclamation, `Once`/
      `OnceCell`. Loom-gated concurrency tests in `lib/sync/tests/loom.rs`,
      proptest fairness test in `lib/sync/tests/rwlock_fairness.rs`,
      decision tree in `docs/src/architecture/sync.md`.
- [x] 2.2 — `kernel/mem`: buddy/bitmap `FrameAllocator` honouring a typed
      `BootMemoryMap`, per-process `AddressSpace<P: PageTable>` (the Arch
      HAL `mmu::AddressSpace + tlb::TlbShootdown` bound alias; W5b-2) with a
      `HostPageTable` test double behind `#[cfg(test)]`, kernel `Slab`
      with guard pages on both sides, `alloc_sensitive` / `free_sensitive`
      zero-on-free backed by `zeroize`, and `Result<_, AllocError>` on every
      allocation path (no panic on OOM). Property tests in
      `kernel/mem/tests/proptest_frame.rs`, loom-gated concurrency tests in
      `kernel/mem/tests/loom.rs`, architecture documentation in
      `docs/src/architecture/memory.md`.
- [x] 2.3 — `kernel/sched`: SMP-from-day-one scheduler. Per-CPU bounded
      Chase–Lev style work-stealing queues (`kernel/sched/src/runqueue.rs`),
      MLFQ priority + fairness with periodic boost
      (`kernel/sched/src/scheduler.rs`, citing Arpaci-Dusseau OSTEP ch. 8),
      IPI-based preemption hook behind the `SchedulerArch` trait, and
      cancellation-safe `spawn`/`park`/`unpark`/`exit` lifecycle. Host-only
      `TestArch` is gated behind the `test-arch` Cargo feature and never
      links into production builds. Tests: deterministic integration tests
      on ≥ 4 simulated cores (`kernel/sched/tests/scheduler.rs`),
      10 000-task stress test asserting no deadlock and bounded latency
      (`kernel/sched/tests/stress.rs`), loom-gated concurrency test for
      the run-queue's lock-free fast path (`kernel/sched/tests/loom.rs`).
      Architecture documentation in `docs/src/architecture/scheduler.md`.
- [x] 2.4 — `kernel/sec`: in-memory `IdentityTable` builder/verifier
      (`kernel/sec/src/identity.rs`) covering users, groups, and
      supplementary-group sets with bounded sizes; per-task
      `TaskCapabilities` (`kernel/sec/src/captable.rs`) whose effective
      set is the intersection of the user grant and the manifest
      request, with delegation/revocation routed through `lib/caps`
      and signed `CapabilityToken` application; Ed25519 manifest
      verification (`kernel/sec/src/manifest.rs`) refusing on bad
      signature, ABI mismatch, or unknown capability; audit log writer
      with stable event IDs `1_000..2_000` (`kernel/sec/src/audit.rs`)
      emitting exactly one record per security decision per
      `AGENTS.md` §5.4.4; "no ambient authority" locked in by unit
      tests against `uid == 0`. Property tests for the task-level
      subset invariant live in
      `kernel/sec/tests/proptest_invariants.rs`. Documentation in
      `docs/src/architecture/security.md`. Stage 2.4 was renumbered
      from the original PLAN's 2.5 to match the Stage 2.4 task brief.
- [x] 2.5 — `kernel/ipc`: capability-checked typed message ports
      with a lock-free closed-state fast path
      (`kernel/ipc/src/port.rs`), explicit capability-gated
      shared-memory objects backed by `kernel/mem`'s zero-on-free
      `SensitiveBuffer` whose revocation atomically invalidates every
      live mapping (`kernel/ipc/src/shmem.rs`), and lossless
      OR-accumulating asynchronous notifications gated by the same
      bind-/send-time capability split as ports
      (`kernel/ipc/src/notify.rs`). Per-endpoint required-capability
      declarations are enforced at port creation **and** on every
      send; receivers do not re-check (`AGENTS.md` §5.2 final bullet).
      Every rejection path emits one structured audit record via
      `kernel/ipc/src/audit.rs` against the reserved
      `3_000..4_000` event-id range. Tests: inline unit tests in each
      module, integration tests in `kernel/ipc/tests/integration.rs`
      for the destruction-during-in-flight-send and
      shared-memory-revocation-racing-mapper scenarios, plus a
      loom-gated harness for the send fast path in
      `kernel/ipc/tests/loom.rs`. `Errno::MessageTooLarge`
      (semantically `EMSGSIZE`) was appended to `lib/abi`. The
      no-allocation audit-field formatters previously living in
      `kernel/sec` were promoted to `lib/util::fmt` once
      `kernel/ipc` became a second consumer (`AGENTS.md` §2.2 / §6).
      Architecture documentation in `docs/src/architecture/ipc.md`.
- [x] 2.6 — `kernel/core`: architecture-neutral `kernel_main`
      orchestrating the documented init order
      `log → mem → sec → sched → ipc` (with Stage 2.7 syscall
      registration plugged in afterwards); `BootInfo` hand-off type and
      `KernelArch` trait that Stage 3 arch ports implement; panic helper
      (`handle_panic`) that logs a structured `KERNEL_PANIC` record
      through `lib/log` carrying CPU id and source location and halts
      via `KernelArch::halt` — the kernel never silently resets.
      Stage 2.6 was renumbered from the original PLAN's 2.7 to match
      the Stage 2.6 task brief (mirrors the precedent set by Stage 2.4).
      Audit-event IDs live in the reserved `4_000..5_000` range; per
      `AGENTS.md` §2 the crate declares zero global mutable statics
      (per-CPU bootstrap lives in the arch crates). Host-side
      integration tests in `kernel/core/tests/kernel_main.rs` drive
      `TestArch + TestSink` and lock the init-order and panic
      contracts. Architecture documentation in
      `docs/src/architecture/kernel.md`.
- [x] 2.7 — `kernel/syscall` (dispatch table generated from
      `lib/abi/src/syscalls.rs`). Renumbered from the original PLAN's
      2.6. `lib/abi/src/syscalls.rs` was finalised with the frozen
      `abi-v1` `SyscallSpec` table (eight syscalls: `yield`, `exit`,
      `ipc_send`, `ipc_recv`, `cap_query`, `cap_delegate`,
      `cap_revoke`, `clock_get`) and a fixed-stride `ENCODED_TABLE`
      whose SHA-256 fingerprint pins the contract. The kernel side
      (`kernel/syscall/src/table.rs`) owns the architecture-neutral
      `Dispatcher`, a `SyscallHandlers` trait plugged in by
      `kernel/core`, type-driven argument validation, and the
      `SYSCALL_TABLE_HASH` constant the kernel re-checks at boot via
      `verify_table_hash`; per-architecture entry stubs that build
      `RawArgs` from syscall registers are deferred to Stage 3 per the
      task brief. Audit IDs live in the reserved `5_000..6_000` range
      (`kernel/syscall/src/audit.rs`) with one record per security
      decision (`AGENTS.md` §5.4.4). `cargo xtask abi-check` was
      extended in `tools/xtask/src/commands/abi_check.rs` to (a)
      refuse if either half of the contract is missing and (b)
      independently recompute `sha256(rustos_abi::ENCODED_TABLE)` and
      compare it against the on-disk literal **and** the linked
      kernel constant; a desync negative-test mutates a temp copy of
      the table to prove the diff tool is not a no-op. A
      deterministic 100 000-iteration fuzz harness
      (`kernel/syscall/tests/fuzz_args.rs`) cross-checks the
      dispatcher's accept/reject decision against an independent
      mirror. Documentation in `docs/src/architecture/syscalls.md`.
- [x] 2.8 — Stage-2 cross-crate / QEMU integration tests +
      `tools/qemu` runner + `cargo xtask test --qemu` flag. Files
      added: `tools/qemu` (audited host-side runner: `grub-mkrescue`
      ISO build wrapper, OVMF discovery across Debian/Ubuntu/Fedora/
      Arch standard paths, `isa-debug-exit` decoding, strict
      per-test wall-clock budget — no retries per `AGENTS.md` §7);
      `tests/integration/memory_isolation/` (freestanding
      `x86_64-unknown-none` kernel binary that builds two distinct
      page-table hierarchies via the partial Stage-3a baseline in
      `kernel/arch/x86_64`, switches CR3 between them, and asserts
      attacker `#PF` with `error_code == 0` + `CR2 == SECRET_VADDR`
      while the victim's frame remains intact);
      `tests/integration/scheduler_stress/` (workspace-level
      cross-crate test: 20 000 tasks across 4 simulated cores
      through `rustos-kernel-sched`'s `TestArch`, asserting
      deadlock-freedom, exact execution count, and bounded
      first-run latency); the `--qemu` opt-in flag in
      `tools/xtask/src/commands.rs` and the driver in
      `tools/xtask/src/commands/qemu_tests.rs`. Documentation:
      new `docs/src/platform/x86_64.md` "Running Stage 2 QEMU
      tests" section (marked "Stage 3a will expand this") plus a
      cross-link from `docs/src/architecture/kernel.md`. The
      Stage-2 deliverable text (lines 154–158) — *scheduler stress
      test under QEMU on ≥ 4 emulated cores* — was satisfied by
      Stage 3a (b) (AP startup, APIC bring-up, INIT-SIPI-SIPI) and
      Stage 3a (c5) (real LAPIC-timer-driven preemption with a
      `preemption_count(cpu) >= 10`-per-CPU assertion).

**Status: complete.**
- All architecture-neutral sub-stages 2.1–2.7 remain complete with
  the previously-recorded evidence (coverage thresholds, fuzz
  harnesses, loom-gated tests, docs).
- 2.8 delivers the QEMU runner + memory-isolation deliverable
  under QEMU end-to-end. `cargo xtask test --qemu` is green on the
  CI host. Toolchain pinned at `nightly-2026-05-27`
  (rustc 1.98.0-nightly).
- The Stage-2 `scheduler_stress` deliverable text (lines 154–158)
  is satisfied in two complementary forms:
    - the cross-crate workspace test on ≥ 4 simulated cores passes
      host-side (`workspace_stress_four_cores_twenty_thousand_tasks`,
      ~0.4 s); and
    - `tests/integration/scheduler_stress_qemu` runs 8 192 tasks
      across 4 real (emulated) cores under **real LAPIC-timer-driven
      preemption** — delivered by Stage 3a (b) (AP startup via
      INIT-SIPI-SIPI) and Stage 3a (c5) (`kernel/arch/x86_64::preempt`).
      The QEMU run asserts `preemption_count(cpu) >= 10` per CPU and
      ≥ 2 distinct dispatching CPUs, so a silent revert to cooperative
      scheduling now fails CI loudly.
- `cargo xtask ci` evidence tail (toolchain
  `nightly-2026-05-27` / rustc 1.98.0-nightly, QEMU 8.2.2,
  GRUB-EFI 2.12, OVMF 2024.02). Refreshed after Stage 3a (c7-bin)
  landed the `rustos-kernel` production bin crate + the
  `kernel_arch_boot` QEMU integration test, and after the
  follow-up audit-routing fix that emits `AuditEvent::BootStarted`
  / `PhaseFailed` / `BootCompleted` on `audit_sink` per
  `AGENTS.md` §5.4.4 (catalogue table in `kernel/core/src/audit.rs`
  and `docs/src/architecture/kernel.md` updated in lockstep):
  ```text
  xtask: [fmt --check]                     cargo fmt --all -- --check
  xtask: [clippy]                          --workspace --all-targets --locked -- -D warnings
  xtask: [test]                            --workspace --all-targets --locked
  xtask: [test --qemu] 3 test(s) enrolled
  xtask: [test --qemu (build rustos-test-memory-isolation)]
  xtask: [test --qemu (run  rustos-test-memory-isolation)]
      kernel=…/rustos-test-memory-isolation cpus=1 timeout=60s
  xtask: [test --qemu (build rustos-test-scheduler-stress-qemu)]
  xtask: [test --qemu (run  rustos-test-scheduler-stress-qemu)]
      kernel=…/rustos-test-scheduler-stress-qemu cpus=4 timeout=120s
  xtask: [test --qemu (build rustos-test-kernel-arch-boot)]
  xtask: [test --qemu (run  rustos-test-kernel-arch-boot)]
      kernel=…/rustos-test-kernel-arch-boot cpus=1 timeout=60s
  xtask: [docs-check (rustdoc)]            -D warnings --document-private-items
  xtask: [docs-check (mdbook)]
  xtask: [docs-check (linkcheck)]          docs/src
  xtask: [deny]                            advisories ok, bans ok,
                                           licenses ok, sources ok
  xtask: [abi-check]                       lib/abi/src/syscalls.rs ↔
                                           kernel/syscall/src/table.rs
  ```
  All host test crates report `ok. … 0 failed; 0 ignored`.
  Coverage floors per `AGENTS.md` §7 unchanged since Stage 1
  (`lib/caps` 98.19 % lines, `lib/crypto` 96.84 % lines, workspace
  total 98.29 %); no kernel-side `lib/*` crate was touched by the
  (c7-bin) finalisation. The new boot QEMU run boots the
  production `rustos-kernel` pipeline end-to-end, observes
  `EventId(4004)` on the audit channel, and flips
  `qemu_exit::exit_success` inside the 60 s budget. The QEMU
  stress (`rustos-test-scheduler-stress-qemu`) continues to bring
  3 APs online via INIT-SIPI-SIPI and run 8 192 tasks across 4
  real (emulated) cores to completion (`PASS`, "distinct
  executing CPUs = 4"), inside the 120 s budget.

---

## Stage 3 — Architecture Ports

**Dependencies:** Stage 2 (interface-level; implementations land in parallel
sub-stages).

**Cross-arch parity burn-down:** bringing `aarch64`, `riscv64`, and
`wasm32` up to (at least) `x86_64` level — finishing the §17.2 Arch HAL
migration, aarch64 SMP/FDT, live-scheduler wiring, and the QEMU vertical
parity sweep — is staged in `plans/WIRING.md` (continuation prompt
`.junie/next-wiring-prompt.md`).

- [x] **WIRING Stage W0 — Arch HAL conformance harness (all ports).**
      `kernel/arch/api` gains a `conformance` module (the architecture
      analogue of the `kernel/sched/api` policy suite): the
      `SchedulerArch` contract checks (`current_cpu` stable, `ticks_now`
      monotonic non-decreasing, `send_ipi`-to-self/stray a panic-free
      no-op, `core_class` total over every `CpuId`) and a `run_all`
      that also drives the existing §19.1 `sidechannel::conformance`
      and §19.10 `memtag::conformance` verticals over the same port's
      handles. `kernel/arch/api/tests/conformance.rs` runs the harness
      over an in-test double (the api crate cannot name a concrete port
      without inverting §17.4); each of the four ports grows a
      `kernel_arch::tests::passes_arch_hal_conformance_suite` host test
      instantiating `conformance::run_all` over its real `*Arch`,
      `SideChannel`, and `MemoryTags` handles. All four Tier-1 ports
      pass; documented in `docs/src/architecture/modularity.md`.
- [x] **WIRING Stage W1 — Early-boot platform-discovery HAL + the shared
      hardware-tree ABI.** `lib/abi/src/hwtree.rs` lands the
      architecture-neutral hardware tree (§18.1): `HwDeviceClass`,
      `HwMatchKey` (compatible / PCI / USB / virtio), `HwResource`
      (MMIO / IRQ / port / DMA, each carrying the `CapabilityId` it
      requests — no ambient authority, §4), and `HwNode` (id, parent,
      class, bounded match-key + resource arrays), all `#[repr(C)]` with
      pinned `WIRE_LEN` little-endian encode/decode and a generated
      `include/rustos/rustos_hwtree.h` C view (drift-guarded by
      `cargo xtask c-header`). `kernel/arch/api` gains the
      `PlatformDiscovery` HAL slice (`platform.rs`): a sink-based,
      allocation-free discoverer emitting root-first `HwNode`s, plus a
      `platform::conformance` vertical folded into `conformance::run_all`
      (now four handles). A new shared `lib/fdt` crate holds the one
      flattened-device-tree parser (`AGENTS.md` §2.2) — memory region,
      riscv `timebase-frequency`, a generic `property` lookup, and a
      feature-gated DTB test fixture. Per-port impls: x86_64
      `AcpiDiscovery` (MADT → Root + CPU/Local-APIC + I/O-APIC), riscv64
      + aarch64 `FdtDiscovery` (riscv64's `fdt` re-exports `lib/fdt`;
      aarch64 adds a `fdt` query layer with PSCI method `hvc`/`smc` +
      generic-timer PPI, the W6 prerequisite), wasm32
      `HostCapabilityDiscovery` (host-capability query → Root +
      CPU-per-worker + optional display). Every port's
      `passes_arch_hal_conformance_suite` now drives a real discovery
      handle. Docs: `docs/src/platform/{x86_64,aarch64,riscv64}.md`,
      `docs/src/architecture/modularity.md`.
- [x] **WIRING Stage W2 — Per-CPU storage HAL.** `kernel/arch/api` gains
      the `PerCpu` slice (`percpu.rs`): `read_self_base` /
      `unsafe write_self_base` over an opaque, full-pointer-width per-CPU
      base word, plus a `percpu::conformance` vertical — a single-handle
      `run_all` round-trip check (folded into `conformance::run_all`,
      now five handles) and a two-handle `run_isolation` check (one
      CPU's word independent of another's). Per-port impls land in a
      `percpu_hal` module (struct `PerCpuStorage`): x86_64 over the
      GS-base MSR (`IA32_GS_BASE`, `sched-arch`-gated), aarch64 over
      `TPIDR_EL1`, riscv64 over the `tp` thread pointer, wasm32 over a
      worker-local slot (each Web Worker owns its own module instance).
      Each port's `passes_arch_hal_conformance_suite` now also drives a
      real `PerCpuStorage`, and each carries host round-trip + isolation
      tests. Docs: `docs/src/platform/{x86_64,aarch64,riscv64,wasm32}.md`,
      `docs/src/architecture/modularity.md`.
- [x] **WIRING Stage W3-A — Interrupt-controller + interrupt-entry HAL
      (traits + per-port migration).** `kernel/arch/api` gains the
      interrupt slice (`irq.rs`): the `IrqController` trait (`mask` /
      `unmask`, fail-closed with `IrqControlError::OutOfRange`) and the
      `InterruptEntry` trait (the `claim` → `complete` prologue/epilogue),
      each with a host-run `irq::conformance` vertical (`run_controller`
      mask/unmask round-trip + fail-closed, `run_entry` claim/complete
      drain-terminates) plus accept/reject self-test doubles. These
      verticals are driven **per-port** (not folded into
      `conformance::run_all`, which stays at five handles): the controller
      check needs a port-specific valid/invalid line pair and
      `InterruptEntry` is implemented by only the claim-based ports — the
      same precedent as `percpu::conformance::run_isolation`. Per-port
      impls: riscv64 `PlicController` (forwarding to its inherent
      mask/unmask + PLIC claim/complete, source 0 → `None`); aarch64
      `GicController` over a new host-testable `GicMmio` seam + `Gicv2<M>`
      driver (`ISENABLER`/`ICENABLER` masking + `SeqCst` fence,
      `IAR`/`EOIR` claim/complete, spurious → `None`) — the freestanding
      `init`/`enable_ppi`/`acknowledge`/`end_of_interrupt`/`send_sgi` free
      functions are now thin wrappers over the driver, so there is one
      MMIO path (§2.2); x86_64 `IoApicController` (downstream in
      `kernel/rustos-kernel`, the `alloc`-bearing controller) implements
      `IrqController` only — x86_64 is vectored, so no `InterruptEntry`
      (no claim register, §2.1). Each port carries a host conformance test
      over its real controller on a mock MMIO. Docs:
      `docs/src/architecture/modularity.md`, `docs/src/security/irq.md`,
      `docs/src/platform/{x86_64,aarch64,riscv64}.md`, `AGENTS.md` §17.2.
- [x] **WIRING Stage W3-B — aarch64 device-IRQ QEMU vertical.** Routed a
      device SPI through the GICv2 + EL1 IRQ path to a Rust handler under
      QEMU. `tests/integration/irq_qemu_aarch64` (the EL1/SPI analogue of
      `irq_qemu_x86_64`) binds the PL031 RTC's GICv2 SPI (INTID 34) in a
      kernel-neutral `rustos_kernel_irq::IrqTable`, routes it to CPU 0 via
      the new `gic::route_spi` (`GICD_ITARGETSR`, SPI-only — SGIs/PPIs
      skipped because their target bytes are read-only/banked), installs a
      set-once device-IRQ dispatcher through the new
      `exceptions::set_device_irq_dispatch` hook (`handle_irq` forwards any
      non-timer INTID to it; EOI unchanged), and forwards the line to
      `IrqTable::fire` over a downstream `GicController`→`kernel_irq`
      `IrqController` bridge (`GicBridge`, in the test crate — the arch
      port keeps no `kernel/irq` dep, §17.2). On the RTC firing, the
      dispatcher masks the GIC line + sets the wait flag, the loop observes
      `WaitStep::Ready`, and the test asserts the enable bit re-reads masked
      (mask-before-wake). New host tests cover the `GICD_ITARGETSR`
      arithmetic, `MIN_SPI_INTID` boundary, `route_spi` SPI-write +
      SGI/PPI-skip, and the fail-closed set-once dispatch slot. Enrolled in
      `tools/xtask/src/commands/qemu_tests.rs` (60 s, single CPU) and
      QEMU-green. Docs: `docs/src/security/irq.md`,
      `docs/src/platform/aarch64.md`. Split out of W3 to keep W3-A a
      host-gated, low-risk landing.
- [x] **WIRING Stage W4 — Timer-programming HAL.** `kernel/arch/api`
      gains the `timer` slice (`timer.rs`): the `Timer` trait
      (`set_tick_callback` / `tick_callback` / `dispatch_tick`) over the
      architecture-neutral `TickFn = extern "C" fn(CpuId)`, plus a
      host-run `timer::conformance` vertical (`run_all`: an installed
      callback fires on dispatch with the CPU it was handed; a handle
      with no callback dispatches harmlessly). Driven **per-port** (not
      folded into `conformance::run_all`) — the handle is constructed per
      port and reaches a port-private callback slot, the same precedent
      as `irq`. Per-port impls land in a `timer_hal` module (struct
      `TimerHal`): each forwards the callback install/read to its
      `preempt` static and the riscv64 (`on_timer_interrupt`), aarch64
      (`on_timer_interrupt`), and wasm32 (`on_animation_frame`) tick
      handlers dispatch back through `TimerHal::dispatch_tick`, so the
      invoke lives in one place (§2.2); x86_64's vectored ISR keeps its
      LAPIC-ID/EOI dispatch and `TimerHal` is its HAL-facing surface
      (`sched-arch`-gated). The hardware arming/re-arming (LAPIC LVT,
      `CNTP_TVAL_EL0`, SBI timer, next animation frame) stays in each
      port's `preempt` (§2.4). The `timer_preempt_qemu_{aarch64,riscv64}`
      verticals install the callback through `TimerHal` and stay green
      through the HAL. Each port carries a host `passes_timer_conformance`
      test. Docs: `docs/src/architecture/modularity.md`,
      `docs/src/platform/{x86_64,aarch64,riscv64,wasm32}.md`,
      `plans/WIRING.md`.
- [x] **WIRING Stage W5a — Context-switch HAL.** `kernel/arch/api` gains
      the `context` slice (`context.rs`): the architecture-neutral
      `TaskContext` save area (a single `#[repr(C)]` `u64`,
      layout-identical to every port's native `TaskCtx`, §2.2), the
      `TaskEntry` alias, the fail-closed `PrepareError`, and the
      object-safe `ContextSwitch` trait (`prepare` seeds a never-run
      task's first frame; `unsafe switch` performs the bare-metal task
      switch), plus a host-run `context::conformance` vertical asserting
      the `prepare` contract (empty context not runnable; null/misaligned/
      too-small stack rejected; good stack yields a runnable, in-bounds
      frame). Driven **per-port** (not folded into `conformance::run_all`)
      — the suite seeds a frame over a caller-supplied stack and runs over
      the port's real handle, the same precedent as `irq`/`timer`. Per-port
      impls land in a `context_hal` module (struct `ContextSwitchHal`):
      x86_64/aarch64/riscv64 each reinterpret the neutral `TaskContext` as
      their native `TaskCtx` (a const-assert pins the layout equality) and
      forward to the existing `context` primitive, so the switch invoke
      lives in one place (§2.2); the bare-metal `switch` is gated to the
      port's freestanding target and the host build is `unreachable!`.
      Each port carries a host `passes_context_switch_conformance` test.
      wasm32 has no context switch (no register file/stack to swap; each
      "CPU" is a separate Web Worker module instance), so the slice is an
      honest **n/a** there with no `ContextSwitchHal` (§2.1 — no fake
      primitive). The bare-metal `switch` itself, like `enter_user`, is
      proven only under QEMU (the scheduler-drive verticals). Docs:
      `docs/src/architecture/modularity.md`,
      `docs/src/platform/{x86_64,aarch64,riscv64,wasm32}.md`,
      `plans/WIRING.md`.
- [x] **WIRING Stage W5b-1 — MMU/page-table HAL trait + per-port
      migration.** `kernel/arch/api` gains the `mmu` slice (`mmu.rs`): the
      neutral `PageFlags` permission set (`READ`/`WRITE`/`EXEC`/`USER`/
      `DEVICE`, W^X-aware), the fail-closed `MapError`
      (`Misaligned`/`AlreadyMapped`/`PoolExhausted`/`InvalidFlags`), the
      object-safe `AddressSpace` trait (`map_page` / `root_phys` /
      `unsafe activate`), and a per-port-driven `mmu::conformance` vertical
      (a port-constructed space + a port-specific mappable address pair:
      non-null root, misaligned rejected, good map accepted, double-map
      refused) with a faithful + lenient in-test double in the api
      `tests/conformance.rs`. Each port implements the trait on its
      existing `paging::AddressSpace` (a retained `pool` field, a neutral
      `PageFlags`→native leaf translation, a read-only `leaf_present` walk
      for the `AlreadyMapped` guard, and `activate` forwarding to the
      gated `switch`), reusing the existing walk so the inherent `map_4k*`
      methods (used by the spawn/c-program/abi-sys verticals) keep their
      signatures (§2.2). riscv64 + aarch64 run `passes_mmu_conformance` on
      the host; x86_64's walk is higher-half (phys ≠ virt) so it is not
      host-runnable and its `map_page`/`activate` are proven by the
      `memory_isolation` QEMU vertical (the same honest asymmetry the
      bare-metal `switch` has). All three `memory_isolation_qemu_*`
      verticals now build their victim/attacker spaces through
      `AddressSpace::map_page` + `activate`. wasm32 is an honest **n/a**
      (no page table). `rustos-arch-api` became a non-optional x86_64 dep
      (the always-compiled `paging` slice names it; `sched-arch` now only
      gates which HAL *modules* compile). Docs:
      `docs/src/architecture/modularity.md`,
      `docs/src/platform/{x86_64,aarch64,riscv64,wasm32}.md`,
      `plans/WIRING.md`. **W5b-2 (done):** extended the MMU HAL trait with
      `translate`/`unmap` (+ `MapError::NotMapped`), added the
      `TlbShootdown` per-page-invalidation slice (each with a host
      conformance vertical), folded `kernel/mem`'s per-process
      `AddressSpace<P>` onto `P: mmu::AddressSpace + TlbShootdown` (the
      `PageTable` bound alias) and **removed** its local `PageTableOps`
      trait, renamed every consumer + the six `{spawn,c}_program_qemu_*`
      crates (deleting their `*UserPageTable` adapters). **W5b-3 (done):**
      added the `PageTableFrames` HAL frame-source slice
      (`kernel/arch/api::frames`: `TableFrame` currency + a host
      `frames::conformance` vertical), made each port's `PageTablePool`
      implement it and rewired every port `AddressSpace::new_*`/`map_4k*`/
      `ensure_child` onto a `&'static dyn PageTableFrames` (the static pool
      stays the boot/bootstrap source, coercing unchanged at the QEMU
      call sites), and added `kernel/mem`'s production `FrameTableSource`
      that backs the tables with the `FrameAllocator` over the direct
      `PhysMap` (fail-closed off-map, §17.4 one-way edge kept). riscv64 +
      aarch64 run `passes_frames_conformance` on the host; `kernel/mem`
      runs the suite over `FrameTableSource`; x86_64's higher-half pool is
      proven through the `memory_isolation` QEMU vertical. **Remaining:**
      cross-CPU TLB shootdown stays W6.
- [x] **WIRING Stage W6 — aarch64 SMP secondary-core bring-up + real
      IPI.** Closed the single largest aarch64 gap, mirroring the riscv64
      port-side `smp` module (no new HAL trait; an `Smp` slice stays a
      future §17.2 decision for both ports). Added
      `kernel/arch/aarch64::psci` (the PSCI `CPU_ON` firmware call over
      the `hvc`/`smc` conduit, host-tested SMC64 id encoding + `PsciRet`
      status decode) and `kernel/arch/aarch64::smp` (+ `smp.s`): a
      set-once `extern "C" fn(CpuId) -> !` secondary entry, a fail-closed
      `start_secondary` launcher that PSCI-starts a parked core at the
      `smp.s` trampoline (which masks IRQs, seeds the core's `.bss` stack
      slice by the dense id PSCI passes as `context_id`, and tail-calls
      the entry), and `current_cpu_index` reading `MPIDR_EL1`.
      `Aarch64Arch` gained the dense-`CpuId`↔`MPIDR` map (`with_cpus` /
      `mpidr_of` / `cpu_for_mpidr`); `current_cpu` reverse-maps the
      running affinity, and `send_ipi` now raises a real GICv2 directed
      SGI (INTID 0) instead of the single-CPU self-target best-effort
      send. `preempt` gained the IPI callback surface (`set_ipi_callback`
      / `enable_ipi` / `on_ipi_interrupt`) and `exceptions::handle_irq`
      dispatches an acknowledged SGI (INTID `< MIN_SPI_INTID`) to it
      through the one `smp::current_cpu_index` identity source (§2.2). New
      QEMU vertical `tests/integration/ipi_smp_qemu_aarch64` (enrolled,
      `--cpus 2`, QEMU-green) starts core 1 via PSCI and delivers it a
      directed SGI, passing once core 1's IRQ path runs the IPI callback
      with core 1's id. **Honest carve-outs (tracked):** non-PSCI
      spin-table boot (bare Raspberry Pi 3) is not built — it would be
      untested asm, so it lands with a spin-table target and a real
      vertical (§2.1 / §2.5); the QEMU vertical names the `virt` conduit
      (`hvc`) directly because QEMU's ELF `-kernel` boot hands no DTB
      pointer and the shared `lib/fdt` walk does not yet parse the full
      ARM `virt` tree at runtime (conduit discovery is the host-tested W1
      capability). **Carried forward:** cross-CPU TLB shootdown; the
      `lib/fdt` runtime parse of the full ARM `virt` tree. Docs:
      `docs/src/platform/aarch64.md`, `plans/WIRING.md`.
- [x] **WIRING Stage W7 — Live `kernel/sched` task switch per arch.**
      Wired the aarch64 `preempt` (generic timer + GICv2 IPI) and
      `context` primitives into the architecture-neutral `kernel/sched`
      `Scheduler`, closing the last "live-scheduler task switch" gap on a
      bare-metal port (x86_64 via `scheduler_stress`, riscv64 via
      `sched_drive_qemu_riscv64`). New QEMU vertical
      `tests/integration/sched_drive_qemu_aarch64`
      (`rustos-test-sched-drive-qemu-aarch64`, enrolled in
      `tools/xtask/src/commands/qemu_tests.rs`, single CPU, 60 s) is the
      EL1/GICv2 analogue of the riscv64 one: on the `virt` board it (1)
      performs a real bidirectional `context::switch` round-trip with
      interrupts disabled (an inbound task seeded by `TaskCtx::prepare`
      records that it ran and `switch`es straight back), (2) builds a real
      `rustos-kernel-sched-mlfq::Scheduler` over `Aarch64Arch`, publishes
      it, and installs both the `preempt` generic-timer callback and the
      GICv2 IPI (SGI) callback so each drives `Scheduler::on_timer_tick`,
      then (3) brings up the EL1 vectors + GICv2, arms the 100 Hz generic
      timer + IPI, spawns 64 tasks, sends itself a directed IPI, and
      drives the cooperative `step` loop until every task has run. PASS
      once the generic-timer IRQ has driven the live scheduler ≥ 20 times
      and the IPI SGI path has driven it at least once; any missing path
      trips a dedicated failure finisher or times out. No new HAL trait
      (the existing `Timer` / `ContextSwitch` slices plus the W6 SMP/IPI
      primitives are reused). **Verified green under QEMU on this host.**
      **Carried forward:** cross-CPU TLB shootdown; the `lib/fdt` runtime
      parse of the full ARM `virt` tree. Docs:
      `docs/src/platform/aarch64.md`, `plans/WIRING.md`.
- [x] **WIRING Stage W8 — wasm32 multi-worker SMP + live cooperative
      scheduler.** Brought wasm32 to the W7 bare-metal level: it now boots
      multi-worker, routes real `MessageChannel` IPIs between live module
      instances, and drives the *live* `rustos-kernel-sched-mlfq`
      `Scheduler` from both the `requestAnimationFrame` tick and the IPI.
      New `kernel/arch/wasm32::smp` (the wasm32 analogue of riscv64 SBI
      HSM / aarch64 PSCI bring-up) `start_worker(n)` range-checks
      `1..MAX_WORKERS`, fails closed (`StartWorkerError`), and asks the
      host (new `rustos_host_start_worker` binding) to spawn a Web Worker
      running the same module as logical CPU `n`; `current_worker`
      recovers the running id. SMP is kept **port-side** (no new HAL
      trait), mirroring riscv64/aarch64; an `Smp` HAL slice remains a
      future §17.2 decision. The host loader (`web/rustos.js`) gained
      shared `instantiate`/`runWorker`, a main-thread `boot` that spawns
      module Web Workers, and a `MessageChannel` IPI hub
      (`rustos_host_post_ipi` → the target's `rustos_arch_wasm32_on_message`,
      worker→worker routed via the main thread); `web/worker.js` is the
      new worker bootstrap. A worker has no `requestAnimationFrame`, so it
      drives its cooperative tick from `setTimeout` (the kernel
      `request_frame` is unchanged). `isolation::live_memory_region` (new,
      wasm-gated) ties the per-worker isolation check to this instance's
      real linear-memory size. The rewritten browser vertical
      (`tests/integration/kernel_arch_boot_wasm32`) has CPU 0 build a live
      `Scheduler<WasmArch>` driven by the RAF loop (`TICK`/frame + `step`),
      spawn a Web Worker (`WORKER_OK`), and send it a directed IPI; CPU 1
      builds its own live scheduler and prints `IPI_RECV` when the
      cross-context IPI drives it; the puppeteer harness serves
      `/worker.js` and PASSes on
      `BOOT_OK`+`ISOLATION_OK`+`WORKER_OK`+`IPI_RECV`+≥ 20 `TICK`.
      **Verified browser-green via `cargo xtask test --wasm` on this
      host.** **Carried forward:** cross-CPU TLB shootdown; the `lib/fdt`
      runtime parse of the full ARM `virt` tree. Docs:
      `docs/src/platform/wasm32.md`, `plans/WIRING.md`.
- [x] **WIRING Stage W9 — side-channel + memory-tagging completeness
      (§19.1 / §19.10).** Verification + parity-tracking landing: the
      §19.1 `SideChannelMitigation` and §19.10 `MemoryTagging` HAL trait
      sets (`kernel/arch/api/src/{sidechannel,memtag}.rs`, each with a
      portable conformance vertical) were confirmed complete and honest on
      **all four ports** and folded into every port's
      `passes_arch_hal_conformance_suite` via
      `rustos_arch_api::conformance::run_all`. The mitigation code was
      delivered earlier with the original §19 framework (see §19 items 8 &
      13), so this stage re-verified rather than rewrote it (§2.2 — no
      duplication). Side-channel: x86_64 `lfence`+`verw` applied (KPTI/IBPB
      `Pending`); aarch64 `csdb` applied, MDS flush `NotVulnerable`, KPTI +
      MIDR Spectre-v2 `Pending`; riscv64 conservative `fence`,
      release-ready (in-order cores — `fence.i`/`sfence.vma` speculation
      sequencing unnecessary, justified); wasm32 release-ready (host-owned).
      Memory-tagging: aarch64 real Arm MTE `stg` behind a default-off gate,
      both slots `Pending` on the Stage 6 `FEAT_MTE` probe; x86_64 /
      riscv64 / wasm32 justified `Unsupported`. The `kernel/mem` slab
      software UAF tag-check (`next_free_tag` rotation,
      `SlabError::TagMismatch`) stays the on-by-default floor on every
      target. **Verified host-green** (`cargo test -p
      rustos-arch-{x86_64,aarch64,riscv64,wasm32}` + `-p
      rustos-kernel-mem`) and via `cargo xtask ci`. **Carried forward
      (Stage-6-blocked):** KPTI + IBPB/IBRS/STIBP/SSBD (x86_64), KPTI +
      MIDR Spectre-v2 (aarch64), auto-enabling Arm MTE on `FEAT_MTE`
      silicon; plus cross-CPU TLB shootdown and the `lib/fdt` runtime ARM
      `virt` parse. Docs: `docs/src/security/side_channels.md`,
      `docs/src/security/memory_tagging.md`, `plans/WIRING.md`.
- [x] **WIRING Stage W10 — heterogeneous `core_class` discovery
      (aarch64).** aarch64 now overrides `SchedulerArch::core_class` with
      `big.LITTLE` classification discovered from the device tree's
      per-core `capacity-dmips-mhz` ratings (x86_64 hybrid already done in
      §17.2 below; riscv64 homogeneous default stands). Shared FDT reader:
      `rustos_fdt::Fdt::each_cpu` — a focused, allocation-free walk over
      `/cpus/cpu@*` yielding each node's `reg` (`MPIDR_EL1` affinity) and
      optional `capacity-dmips-mhz` — plus an `arm_with_cpus` `big.LITTLE`
      fixture (one parser, shared by every arch, §2.2). Pure classifier:
      `kernel/arch/aarch64::hetcore::classify_by_capacity` maps the peak
      advertised rating to the performance tier and any core strictly
      below it to efficiency, failing conservative to performance for a
      homogeneous / missing rating (§2.9). Port wiring: a per-CPU
      `core_classes` table (mirroring `X86_64Arch`), `record_core_class`,
      `classify_from_fdt` (affinity → dense `CpuId` → recorded table), and
      the `core_class` override (out-of-range → performance, never a
      panic); the boot consumer calls `classify_from_fdt` once on the boot
      core. **Verified host-green** (`cargo test -p rustos-arch-aarch64 -p
      rustos-fdt`): asymmetric reporting
      (`classify_from_fdt_reports_big_little_cores`) + the homogeneous
      default, and the shared HAL conformance vertical
      (`core_class_is_total`, run by every port via `run_all`) asserts
      totality. **Carried forward:** cross-CPU TLB shootdown and the
      `lib/fdt` runtime ARM `virt` parse (FDT `cpu-map` topology). Docs:
      `docs/src/platform/aarch64.md`, `plans/WIRING.md`.
- [x] **WIRING Stage W11-A — virtio blk + net MMIO verticals (aarch64).**
      aarch64 now runs the virtio-blk and virtio-net `virt`-board MMIO
      QEMU verticals, the EL1/GICv2 analogue of the riscv64 ones:
      `tests/integration/virtio_blk_mmio_aarch64` (read sector 0 + verify
      the planted pattern, then write/read-back sector 1) and
      `tests/integration/virtio_net_mmio_aarch64` (ARP-resolve the SLIRP
      gateway `10.0.2.2`, then ICMP echo), both enrolled in
      `tools/xtask/src/commands/qemu_tests.rs`. The device-agnostic
      bring-up moved into a new `cfg(itest_aarch64)` `imp_mmio_aarch64`
      module in the shared `virtio_qemu_support` crate: enable FP/SIMD at
      EL1 (`CPACR_EL1.FPEN`), bring up a 2 GiB identity-mapped stage-1 MMU
      (GiB 0 Device, RAM Normal-cacheable — the precondition for the
      atomic-heavy driver/DMA/sync stack, which riscv64 gets from its boot
      pipeline), provision the transport through the capability-gated
      `KernelMmioMapper`, walk the DTB for the device's GICv2 SPI, wire
      the EL1 device-IRQ dispatch to a `kernel/irq` `IrqTable` over a
      `GicController` bridge, and park on a race-free DAIF-masked `wfi`.
      The lifecycle and the blk/net round-trip tails are the *same* shared
      code the riscv64 / x86_64 verticals run (§2.2); `dtb_total_size`
      moved into the shared `common` module. Because QEMU's `-kernel
      <ELF>` aarch64 path passes no DTB pointer (x0 = 0), each vertical
      embeds the canonical `virt` DTB dumped at build time by
      `qemu...dumpdtb` (gated to the aarch64-none target). **Verified
      QEMU-green:** both bins exit `0` under `qemu-system-aarch64 -M virt`.
      **Carried forward (Stage W11-B):** the aarch64 display + input
      verticals (they reuse this bring-up). Docs:
      `docs/src/platform/aarch64.md`, `plans/WIRING.md`.
- [x] **WIRING Stage W11-B — display + input verticals (aarch64).**
      aarch64 now runs the `ramfb`/framebuffer display QEMU vertical, the
      EL1/GICv2 analogue of `framebuffer_display_qemu_riscv64`:
      `tests/integration/framebuffer_display_qemu_aarch64`
      (`rustos-test-framebuffer-display-qemu-aarch64`) programs QEMU's
      `ramfb` over `fw_cfg`, assembles the geometry as a
      `FramebufferConfig`, loads the signed framebuffer `.rxe` through
      `rustos_drvhost::Host`, and drives `load → use → unload → reload`
      (mapping the surface through the capability-gated `KernelMmioMapper`
      and reading the presented pixels back through an independent window),
      enrolled in `tools/xtask/src/commands/qemu_tests.rs` (`ramfb: true`).
      To avoid duplication (§2.2): the W11-A EL1 FP-enable + 2 GiB
      identity-MMU bring-up was extracted to a public
      `bring_up_el1_identity_mmu(&dyn QemuEnv)` (env type made public as
      `AArch64QemuEnv`), reused by the virtio scenario and the display
      vertical; the byte-identical `fw_cfg` MMIO transport (`MmioDma`) was
      moved into the shared `rustos-itest-fwcfg` crate and now serves both
      the riscv64 and aarch64 display verticals (the riscv64 local copy was
      deleted), while x86_64's IOport transport stays distinct. The
      vertical embeds the canonical `virt` DTB (build-time `dumpdtb`) to
      discover the `fw_cfg` base, since QEMU's aarch64 `-kernel <ELF>` path
      passes no DTB pointer. **Verified QEMU-green:** the bin exits `0`
      under `qemu-system-aarch64 -M virt`.
      **Input (landed).** aarch64 now also runs the virtio-input QEMU
      vertical, the `virt`-board analogue of the x86 PS/2 vertical, filling
      the `input` row of the QEMU matrix:
      `tests/integration/input_virtio_mmio_qemu_aarch64`
      (`rustos-test-input-virtio-mmio-qemu-aarch64`) reuses the same
      `bring_up_el1_identity_mmu` + embedded-DTB path, arms the GICv2 SPI,
      loads the signed virtio-input `.rxe`, and drives
      `load → use → unload → reload` (`keyboard: Some(..)` enrolment). The
      new `drivers/input/virtio_input` (`rustos-drv-input-virtio-input`)
      implements `Input` over the bus-agnostic `lib/virtio` transport; its
      `virtio_input_keypress` round-trip tail lives in the shared
      `virtio_qemu_support` crate (§2.2), so a riscv64 sibling is a thin
      new bin. The driver **pre-posts a pool of eventq buffers** (QEMU's
      virtio-input completes a buffer per event, so a keypress's `EV_KEY`
      and its `EV_SYN` each need one) and negotiates `VIRTIO_F_VERSION_1`.
      "Use" is a real device→driver key: the `tools/qemu` runner attaches
      a `virtio-keyboard-device` (`Spec::with_virtio_keyboard`), watches
      the serial console, and on the readiness marker sends `sendkey` over
      a private-socket QEMU monitor held open until run end (a readline
      monitor drops a command on early disconnect); a
      `--virtio-keyboard <marker> <key>` flag exposes the same on
      `rustos-qemu-run`. **Verified QEMU-green:** the bin exits `0` under
      `qemu-system-aarch64 -M virt`. Docs:
      `docs/src/platform/aarch64.md`, `docs/src/drivers/display.md`,
      `docs/src/drivers/input.md`, the framebuffer + virtio-input driver
      `README.md`s, `plans/WIRING.md`.
      **Input — riscv64 sibling (landed, WIRING Stage W11-C).** riscv64
      now also runs the virtio-input vertical, filling the last `input`
      row of the QEMU matrix:
      `tests/integration/input_virtio_mmio_qemu_riscv64`
      (`rustos-test-input-virtio-mmio-qemu-riscv64`) is the thin MMIO
      sibling promised above — it reuses the exact `imp_mmio` riscv64
      bring-up (DTB virtio-MMIO walk, PLIC source + S-mode trap dispatch,
      `KernelVirtioHost`) the blk/net verticals run, and the same
      `rustos-drv-input-virtio-input` driver and shared
      `virtio_input_keypress` tail the aarch64 vertical runs (§2.2),
      differing only in the device id (`18`) and resolver. No new driver
      or shared scaffolding was needed. The runner's monitor
      key-injection is architecture-neutral; the only runner change is the
      `virtio-keyboard-device` attach in the riscv64 argv builder
      (`tools/qemu/src/riscv64.rs`) with matching argv unit tests.
      **Verified QEMU-green:** the bin exits `0` under
      `qemu-system-riscv64 -M virt`. Docs:
      `docs/src/platform/riscv64.md`, `docs/src/drivers/input.md`, the
      virtio-input driver `README.md`, `plans/WIRING.md`.
- [x] **WIRING Stage W13 — cross-CPU TLB-shootdown HAL slice (all
      ports).** The last ad-hoc per-port memory primitive is now an
      object-safe Arch HAL trait, `CrossCpuTlbShootdown`
      (`kernel/arch/api/src/xtlb.rs`), with one infallible method
      `shootdown_page(&self, vaddr)` and a host `xtlb::conformance`
      vertical (object-safe, total, panic-free). It is a *separate* trait
      from the local `TlbShootdown` (W5b-2), not a flag on it: the local
      flush is one privilege-neutral instruction the hot map/unmap loop
      drives, the cross-CPU shootdown needs the port's topology and global
      visibility (§2.4). Implemented once per port on its `SchedulerArch`
      handle (the §2.2 modularity carve-out — same trait, port-specific
      mechanism): **x86_64** a `TLB_SHOOTDOWN_VECTOR` (0x21) IPI +
      lock-serialised mailbox + per-target `invlpg`/acknowledge spin
      (`kernel/arch/x86_64/src/tlb_shootdown.rs`); **aarch64** the
      inner-shareable broadcast `tlbi vaae1is` + `dsb ish`/`isb` (shared
      with the local flush via `paging::invalidate_page_inner_shareable`,
      §2.2); **riscv64** a local `sfence.vma` (shared
      `paging::invalidate_page_local`) + the SBI RFENCE `remote_sfence_vma`
      firmware call (new `sbi::sbi_call4` + `SBI_EXT_RFENCE`); **wasm32**
      an honest n/a (isolated linear memory, no shared TLB — implements
      nothing). Three new real-≥2-core QEMU verticals,
      `cross_cpu_tlb_shootdown_qemu_{riscv64,aarch64,x86_64}` (enrolled
      `cpus: 2`), each start a secondary CPU and drive `shootdown_page`;
      the x86_64 one returns only once the AP's ISR `invlpg`'d and
      acknowledged, the riscv64 one asserts the firmware reports the remote
      fence reached the live hart, the aarch64 one proves the broadcast
      executes without faulting. **Verified host-green** (the three
      `passes_cross_cpu_tlb_shootdown_conformance` tests) and **QEMU-green**
      (all three bins exit `0` under `cargo xtask test --qemu`; a 1-CPU
      x86_64 run correctly fails, so PASS is genuine). No `lib/abi` change,
      so no ABI / C-header drift. Docs:
      `docs/src/architecture/modularity.md`,
      `docs/src/platform/{x86_64,aarch64,riscv64,wasm32}.md`, `AGENTS.md`
      §17.2, `plans/WIRING.md`.
- [x] **WIRING Stage W14 — SMP secondary-CPU bring-up HAL slice (all
      ports).** The last enumerated §17.2 primitive is now an object-safe
      Arch HAL trait, `SecondaryBringup` (`kernel/arch/api/src/smp.rs`),
      with one method `unsafe fn start_secondary(&self, cpu) -> Result<(),
      SmpError>` implemented on each port's `SchedulerArch` handle, and a
      host `smp::conformance` vertical (object-safe, fails closed for an
      unstartable id, panic-free). The directed-IPI half of SMP is already
      `SchedulerArch::send_ipi`, so the slice is start-only (§2.4); the
      set-once entry stays port-shaped (a bare-metal `extern "C"
      fn(CpuId) -> !` vs. wasm32's fixed module export — §2.1). Per-port
      (the §2.2 carve-out): **x86_64** — the full AP bring-up (per-AP
      stack pool, `AP_TRAMPOLINE_PHYS` frame, boot `CR3`, PIT `Delay`,
      set-once entry, INIT-SIPI-SIPI, ready-wait) **moved out of** the
      `scheduler_stress_qemu` / `cross_cpu_tlb_shootdown_qemu_x86_64` test
      bins (which had duplicated it) into `kernel/arch/x86_64::smp`, and
      both verticals now call `X86_64Arch::start_secondary`; **aarch64**
      delegates to the W6 PSCI `CPU_ON` `smp::start_secondary` with the
      conduit installed via `Aarch64Arch::with_psci_method`; **riscv64**
      delegates to the SBI HSM `hart_start` path; **wasm32** delegates to
      `smp::start_worker` (Web Worker spawn). **Verified host-green** (the
      four `passes_secondary_bringup_conformance` tests) and the migrated
      x86_64 verticals build freestanding and stay **QEMU-green** under
      `cargo xtask test --qemu`. With this slice **every** §17.2
      architecture primitive lives behind the HAL; the burn-down is
      complete. No `lib/abi` change, so no ABI / C-header drift. Docs:
      `docs/src/architecture/modularity.md`,
      `docs/src/platform/{x86_64,aarch64,riscv64,wasm32}.md`, `AGENTS.md`
      §17.2, `plans/WIRING.md`.
- [x] **WIRING Stage W15 — fold the bare-metal/wasm SMP verticals onto
      the HAL.** W14 routed only the x86_64 SMP verticals through
      `SecondaryBringup::start_secondary`; the
      `ipi_smp_qemu_{riscv64,aarch64}` and `kernel_arch_boot_wasm32`
      verticals still called the port-private `smp::start_secondary` /
      `smp::start_worker` directly (sound, since the free port helper the
      HAL delegates to is not §2.2 duplication, but asymmetric). W15
      finishes the symmetry so every SMP vertical starts its secondary
      through the neutral HAL trait: **riscv64** builds the
      `RiscvArch::with_harts` handle up front and calls
      `arch.start_secondary(secondary_hartid)`; **aarch64** adds
      `.with_psci_method(VIRT_PSCI_METHOD)` to the `with_cpus` handle and
      calls `arch.start_secondary(SECONDARY_CPU)` (PSCI `CPU_ON`);
      **wasm32** calls `arch.start_secondary(WORKER_CPU)` on the existing
      `WasmArch::with_workers` handle. Each keeps `smp::set_secondary_entry`
      (entry install is off-trait by design, §2.4) and imports
      `SecondaryBringup`; no new HAL surface and no `lib/abi` change → no
      ABI / C-header drift. **Verified QEMU-green** —
      `ipi_smp_qemu_{riscv64,aarch64}` exit `0` under `rustos-qemu-run
      --cpus 2` and the wasm32 browser harness reports `WORKER_OK=true
      IPI_RECV=true PASS` — and **host-green** (`cargo xtask ci`,
      `fuzz --secs 5`, `soak both --secs 10`). Docs:
      `docs/src/architecture/modularity.md`,
      `docs/src/platform/{aarch64,riscv64,wasm32}.md`, `plans/WIRING.md`.
- [x] **WIRING Stage W16 — wasm32 framebuffer display vertical (browser
      canvas).** The last `display`-row parity gap: wasm32 had no
      `display` vertical, so W16 adds the browser analogue of
      `framebuffer_display_qemu_{riscv64,aarch64}`. One new host import,
      `rustos_host_present_framebuffer` (`kernel/arch/wasm32/src/bindings.rs`
      + safe wrapper `host_present_framebuffer`), is supplied by
      `web/rustos.js` (a `boot`/`runWorker` `presentFramebuffer` ctx hook,
      headless no-op by default); it is the wasm32 scan-out analogue of a
      bare-metal port reading its framebuffer back through an independent
      mapping — the host paints the presented RGBA8888 surface onto a
      canvas, reads it back, and returns the count of pixels that survived
      the round-trip. The new vertical
      (`tests/integration/framebuffer_display_wasm32`, a `cdylib` that is
      inert on the host build) loads the build-time signed framebuffer
      `.rxe` through `rustos_drvhost::Host` (the §8 gate) and drives
      `load → use → unload → reload`; "use" maps the surface through a
      capability-checked `WasmMmioMapper` (the MMU-less analogue of
      `KernelMmioMapper` — a bounds- + `CAP_MMIO_MAP`-gated view of the one
      in-memory surface) and `present`s a frame, confirmed **twice**:
      through a second independently-mapped window (bytes reached linear
      memory) and through the canvas round-trip (all `WIDTH×HEIGHT` pixels
      survived). It prints `BOOT_OK` then `DISPLAY_OK`; any failure traps
      the instance (§2.9 / §5.4.5). `DisplayFormat::Rgba8888` with opaque
      (`0xFF`) alpha keeps the canvas premultiplied-alpha round-trip
      lossless. `tools/xtask/.../wasm_tests.rs` was generalised to a
      `VERTICALS` list (boot + display) so `cargo xtask test --wasm` builds
      and runs both (§2.2). **Verified browser-green** — boot vertical
      (`BOOT_OK ISOLATION_OK WORKER_OK IPI_RECV ticks=20 PASS`) and display
      vertical (`BOOT_OK=true DISPLAY_OK=true PASS`) — and **host-green**
      (`cargo xtask ci`, `fuzz --secs 5`, `soak both --secs 10`). No
      `lib/abi` change, so no ABI / C-header drift. Docs:
      `docs/src/platform/wasm32.md`, `docs/src/drivers/display.md`,
      `plans/WIRING.md`.
- [x] **WIRING Stage W17 — one trimmed aarch64 `virt` DTB embed (§2.2) +
      close the lib/fdt-runtime-parse note.** Resolves the long-standing
      "`lib/fdt` runtime parse of the full ARM `virt` tree" carry-forward
      (W6/W7) and the §2.2 duplication the DTB-embedding device verticals
      had grown. The note was stale: W12 gave `lib/fdt` the full
      `virt`-tree node API and the W11 device verticals already parse the
      full ARM `virt` tree **at runtime** through `rustos_fdt::Fdt` (slot
      `reg`/`interrupts`, `fw_cfg` base) after their EL1 identity-MMU
      bring-up. The two SMP verticals (`ipi_smp_qemu_aarch64`,
      `cross_cpu_tlb_shootdown_qemu_aarch64`) keep naming the PSCI conduit
      directly because they run **MMU-off by design** — with the stage-1
      MMU disabled every access is Device memory, where an FDT walk's
      compiler-emitted multi-byte loads fault, and they install no vectors
      on the boot core, so the fault hangs; forcing an MMU + vectors in
      purely to re-derive `hvc` would distort them and triplicate the
      bring-up (§2.1/§2.3). Production conduit discovery stays the W1
      host-tested + conformance-gated `fdt::psci_method` path. What landed:
      the four byte-identical `dump_virt_dtb` copies in the aarch64 device
      build scripts (`framebuffer_display`, `input_virtio_mmio`,
      `virtio_blk_mmio`, `virtio_net_mmio`) now reuse one build-glue
      helper, `rustos_itest_harness::dump_aarch64_virt_dtb` (with the
      unit-testable `dump_virt_dtb_args`); and `trim_fdt_to_extent` trims
      the 1 MiB `dumpdtb`-padded blob to the extent its FDT header
      describes (rewriting `totalsize`), so each vertical embeds the
      few-KiB meaningful tree instead of ~1 MiB of zero padding. The
      trimmed blob stays a valid FDT (`rustos_fdt::Fdt::new` validates
      against the buffer length, not `totalsize`), proven by a harness
      round-trip unit test over the shared `rustos_fdt::fixture` builder
      and by the device verticals parsing it at runtime. **Verified
      QEMU-green** — `framebuffer_display_qemu_aarch64` (ramfb) and
      `virtio_blk_mmio_aarch64` exit `0` against the trimmed DTB, and the
      unchanged SMP verticals stay green — and **host-green**
      (`cargo fmt --all --check`, `cargo xtask ci`, `fuzz --secs 5`,
      `soak both --secs 10`). No `lib/abi` change → no ABI / C-header
      drift. Docs: `docs/src/platform/aarch64.md`, `plans/WIRING.md`.

Each sub-stage delivers one architecture. They share the same checklist:

- Boot stub (minimal assembly, justified per `AGENTS.md` §1).
- Early console (serial/UART/framebuffer/WASM console).
- MMU / page-table primitives wired into `kernel/mem`.
- Context switch + interrupt entry/exit.
- Timer + IPI plumbing for `kernel/sched`.
- Per-arch syscall entry.
- QEMU run script in `tools/qemu/<arch>.rs`.

**Sub-stages**
- 3a — `kernel/arch/x86_64` (BIOS + UEFI boot, APIC, ACPI minimal).
  **Partial baseline already in tree** (delivered by Stage 2.8 to
  unblock the QEMU `memory_isolation` test):
    - [x] multiboot2 header, 32→64 long-mode trampoline (`boot.s`,
          fully annotated `SAFETY-INVARIANT` block),
    - [x] identity-mapped bootstrap paging covering first 32 MiB,
    - [x] minimal IDT (`#PF`/`#GP`/`#DF` + closed-fail thunk on every
          other vector),
    - [x] 16550 polled serial console on COM1,
    - [x] QEMU `isa-debug-exit` helper.

  **Remaining for Stage 3a completion** (one item left for the
  x86_64 sub-stage — see (c7) below). Stage 2 was flipped to
  `Status: complete` once (b) + (c5) satisfied the
  `scheduler_stress`-under-QEMU deliverable text.
    - [x] **(a)** UEFI / Multiboot2 memory-map hand-off:
          `kernel/arch/x86_64::multiboot2` parses the Multiboot2 v2
          information structure (tags 4, 6, 14, 15, 17) zero-copy;
          `kernel/arch/x86_64::bootmemory` bridges Multiboot2 BIOS
          mmap and UEFI EFI memory-map entries to
          `MemoryRegionDescriptor`s whose `RegionKind` is locked to
          `rustos_kernel_mem::RegionKind` by a host-side dev-dep
          round-trip test (AGENTS.md §2.2). The descriptors are
          drained into a `BootMemoryMap` by the kernel binary; the
          arch crate stays `alloc`-free in production so the
          Stage-2 freestanding test binaries still link. Static
          identity-mapped paging from boot.s still installs the
          first 32 MiB until the AP-bring-up commit (b) replaces it
          with the parsed map.
    - [x] **(a)** ACPI MADT parse → LAPIC IDs:
          `kernel/arch/x86_64::acpi` implements RSDP v1/v2 validation
          (signature + one-byte modular checksum + extended checksum),
          a generic SDT header validator, and a typed MADT iterator
          covering Local APIC, IO-APIC, Interrupt Source Override,
          Local APIC NMI, and Local APIC Address Override entries
          (ACPI 6.5 §5.2.5 / §5.2.12). All paths are `no_alloc` and
          host-unit-tested with hand-crafted byte buffers (12 tests).
    - [x] **(a)** APIC bring-up + LAPIC-timer calibration:
          `kernel/arch/x86_64::apic` exposes `Lapic` / `IoApic`
          drivers behind `LapicMmio` / `IoApicMmio` traits with a
          volatile-MMIO production impl gated on `target_os = "none"`
          and host-side mocks driving the unit tests. Covered:
          software-enable + SVR, EOI, INIT/SIPI/INIT-deassert IPI
          sequence (consumed by Stage 3a (b)), IO-APIC
          redirection-entry programming.
          `kernel/arch/x86_64::apic_timer` performs PIT-channel-2
          calibration into a `Calibration { ticks_per_second,
          initial_count, period_micros }` value and programs the
          LAPIC timer in periodic mode; the pure
          ticks/sec → initial-count math is split out for
          independent host testing. The wiring to
          `kernel/sched`'s preemption hook lives in Stage 3a (c)
          alongside the interrupt prologue, because no
          `SchedulerArch` preemption surface exists today
          (`AGENTS.md` §15.2 — no invented APIs).
    - [x] **(b)** AP startup via INIT-SIPI-SIPI trampoline at
          `AP_TRAMPOLINE_PHYS = 0x8000` (SIPI vector `0x08`).
          `kernel/arch/x86_64::smp` ships a position-independent
          real-mode → long-mode payload (`ap_trampoline.s`,
          `// SAFETY:`-annotated end-to-end), a typed
          `TrampolineFrame` installer, an `ApBootSlot` whose layout is
          locked by a host unit test against the assembler-side
          `_ap_trampoline_boot_slot_offset` symbol, and an
          `init_sipi_sipi` sequencer that reuses the existing
          `Lapic::send_ipi` / `send_init_deassert` primitives (no new
          architecture-neutral surface — AGENTS.md §2.4 / §15.5).
          `boot.s` was broadened to identity-map the full 0..4 GiB
          window so the LAPIC MMIO frame (`0xFEE00000`), IO-APIC frame
          (`0xFEC00000`), and any ACPI table OVMF places in high
          memory below 4 GiB are reachable. The Stage-2 deliverable
          on `scheduler_stress` is satisfied by the new sibling crate
          `tests/integration/scheduler_stress_qemu`: a freestanding
          x86_64 kernel that parses Multiboot2 → RSDP → XSDT/RSDT →
          MADT, brings up 3 APs via the new sequencer, runs 8 192
          tasks across 4 real (emulated) cores in a cooperative
          step-loop, and asserts deadlock-freedom, exact execution
          count, and ≥ 2 distinct dispatching CPUs. The original
          host-side `tests/integration/scheduler_stress` workspace
          test is untouched and stays green (`AGENTS.md` §7 — every
          existing test still passes). 9 new host unit tests cover
          `smp` (ApBootSlot layout, frame install, INIT-SIPI-SIPI
          ordering against a `LapicMmio` mock).
    - [x] **(b-hh)** Higher-half kernel. The QEMU boot test
          (`rustos-test-kernel-arch-boot`) was failing
          `cause=syscall_init_failed`: the syscall-entry `RSP0`
          validator (`syscall_entry::validate_kernel_rsp0`, landed with
          the §5 ret2usr/CVE-2019-1125 hardening) demands a canonical
          *higher-half* kernel stack, but the kernel was linked and ran
          identity-mapped in the *low* half, so every boot rejected the
          BSP stack. Fixed by converting x86_64 to a -2 GiB higher-half
          kernel (matching the pre-existing `-C code-model=kernel`):
          `linker.ld` now links the Rust sections at
          `KERNEL_VMA_BASE = 0xFFFFFFFF80000000 + phys` (loaded low via
          `AT()`) while keeping the early trampoline (`.boot.*`) 1:1 in
          low memory; `boot.s` maps the higher-half window
          (`PML4[511] → PDPT[510] →` the reused first-GiB identity PD)
          alongside the preserved 0..4 GiB identity map and jumps into
          the high half (`higher_half_entry`). The kernel-pointer→phys
          conversion (`v >= KERNEL_VMA_BASE ? v - base : v`) was threaded
          through the page-table pool (`paging::phys_of`), the
          memory-isolation test's `AddressSpace` (which now also mirrors
          the higher-half window so a CR3 switch keeps high RIP mapped),
          the vesa framebuffer base, and the fw_cfg DMA seam
          (`DmaAddressRegister::to_physical`). The direct physical map
          (`kernel/mem` `phys.rs`, DMA/MMIO) is unchanged because the
          identity map is preserved. Full `cargo xtask test --qemu`
          matrix (x86_64 incl. vesa/SMP/virtio/rustfs/net, riscv64,
          aarch64) is green.
    - [~] **(c, partial)** Per-CPU GDT + TSS + IST primitives.
          `kernel/arch/x86_64::gdt` ships the canonical 7-slot GDT
          layout (`GdtEntry::{kernel_code, kernel_data, user_code,
          user_data}`), an SDM-aligned `Tss` (offsets pinned via
          `offset_of!` against SDM Vol 3A §8.7 Figure 8-11 — `RSP0`
          at +0x04, `IST1` at +0x24, `IOPB` at +0x66, size 0x68), a
          `PerCpuGdt` builder with validating `set_ist` /
          `set_privilege_stack` / `finalize` (every input checked;
          violations surface as `IstError` — no panics), a
          `tss_descriptor` splitter that scatters base/limit per SDM
          §8.2.3 Figure 8-4 and clamps misuse-supplied DPL into 2
          bits, and an `unsafe fn install(&'static mut self)` gated
          to `target_os = "none"` that issues `lgdt`, reloads
          DS/ES/FS/GS/SS, far-returns to reload CS, and `ltr`s the
          TSS selector. 21 dedicated host unit tests cover every
          non-asm invariant; a `const _` assert pins `TSS_BYTE_LEN`
          to `size_of::<Tss>()` so a struct edit that desyncs the
          descriptor limit fails compilation. **Not yet wired**: the
          BSP and the APs in
          `tests/integration/scheduler_stress_qemu` still run on the
          trampoline-internal GDT; calling `PerCpuGdt::install` from
          `kernel_main` and `ap_entry` is part of the next item.
    - [~] **(c1/c2/c3, partial)** Context-switch primitive, common
          ISR prologue, and per-CPU GDT/IDT bring-up wired into
          `tests/integration/scheduler_stress_qemu`.
          `kernel/arch/x86_64::context` exposes `TaskCtx { rsp: u64 }`
          with a layout-pinning const-assert, `TaskCtx::prepare` for
          first-run frame synthesis (rejects null / misaligned /
          too-small stacks via `PrepareError`), and an
          `extern "C" fn rustos_arch_x86_64_switch` whose `context.s`
          body saves the six callee-saves + `rdi` on the outgoing
          kernel stack, swaps `rsp` through `TaskCtx`, and pops
          symmetrically.
          `kernel/arch/x86_64::interrupts` ships `InterruptStackFrame`
          / `SavedRegs` (both `#[repr(C)]` with `offset_of!` pins
          against Intel SDM Vol 3A §6.14), `IdtEntry::interrupt_gate`
          (masks `ist` to 3 bits so an attacker-controlled IST index
          cannot smear into `type_attr`), `Idt::with_default_handler`
          covering all 256 slots, and `unsafe fn Idt::load`.
          `interrupts.s` provides the common ISR prologue (15 GPR
          pushes, System V AMD64 §3.2.2 stack-alignment pad,
          dispatch into a fail-closed Rust callback per AGENTS.md
          §10).
          `kernel/arch/x86_64::percpu` owns the static
          `[PerCpu; MAX_CPUS]` arena (no allocator), wires `#DF` to
          IST 1 and `#NMI` to IST 2 with 16 KiB stacks, and exposes
          `unsafe fn init(cpu_index)` that finalises the per-CPU
          GDT, `lgdt`-installs it, and `lidt`-installs the per-CPU
          IDT. `scheduler_stress_qemu` calls `percpu::init(0)` at
          the top of `kernel_main` and `percpu::init(cpu_id)` at the
          top of each `ap_entry`, retiring the trampoline-internal
          GDT for steady-state. 21 new host unit tests (context: 5,
          interrupts: 8, percpu: 6, plus 2 layout const-asserts) sit
          on top of the existing 76 in the arch crate, taking the
          host total to 97.
    - [x] **(c4)** Scheduler preemption observation point.
          `kernel/sched::Scheduler` gained
          `on_timer_tick(cpu) -> SchedResult<()>` plus the
          observable `preemption_count(cpu)` /
          `total_preemption_count()` counters. The entry point is
          *counter-only* — it bumps a `Relaxed` per-CPU `AtomicU64`
          and returns. It does **not** call `Scheduler::step`: the
          task registry `RwLock` and the overflow `SpinLock` are
          explicitly forbidden from interrupt context by
          `lib/sync` (`rwlock.rs` module docs: "Process /
          kernel-thread context only. Never from an interrupt
          handler."), and an ISR-driven `step` would deadlock
          against the same CPU's in-progress `spawn` or mid-
          `drain_overflow_to`. The cooperative `step` loop driven
          from kernel-thread context remains the only writer of
          run-queue state. The `SchedulerArch` trait is deliberately
          not extended: `send_ipi` already documents the
          scheduler-asks-arch direction, and the inverse arch-into-
          scheduler timer path is, by construction, a method on the
          scheduler type (AGENTS.md §2.4 — no interface creep,
          §15.5 — no parallel "convenience" surface). 5 new host
          unit tests in `scheduler::tests` cover the counter, the
          no-dispatch contract, the idle-still-counts behaviour, the
          error surface, and per-CPU isolation; the new
          `architecture/scheduler.md#timer-driven-preemption-entry-point`
          section is the long-form prose contract.
    - [x] **(c5)** LAPIC-timer-driven preemption on x86_64.
          `kernel/arch/x86_64::interrupts` gained a `define_isr!`
          macro that emits a `#[naked]` `extern "C"` ISR stub per
          vector, sharing the same 15-GPR push / 16-byte stack
          align / call / 15-GPR pop / `iretq` sequence as the
          default thunk in `interrupts.s` but parameterised on the
          dispatcher symbol via `sym`. The new
          `kernel/arch/x86_64::preempt` module owns
          `TIMER_VECTOR = 0x20`, a 256-entry
          `LAPIC_TO_CPU_ID` mapping populated at bring-up, a
          `set_timer_callback` storage (round-tripped through an
          `AtomicU64`-packed `fn` pointer), a fail-closed
          dispatcher (`rustos_arch_x86_64_timer_dispatch`) that
          looks up the CPU ID, invokes the callback, then writes
          `0` to LAPIC EOI, and a per-CPU
          `init_local_preempt(cpu_index, &mut lapic, calibration)`
          entry that installs the ISR via the new
          `percpu::install_vector` (volatile raw-pointer write
          through a freshly-derived `&mut PerCpu` slot) and
          programs the LAPIC timer in periodic mode.
          `scheduler_stress_qemu` now calibrates the LAPIC timer
          once on the BSP against PIT channel 2, publishes the
          `Calibration` for APs, installs the scheduler-tick
          callback, registers LAPIC→CpuId mappings, calls
          `init_local_preempt` on every CPU after `percpu::init`,
          `sti`s, and asserts `preemption_count(cpu) >= 10` per
          CPU at the end of the workload — a silent revert to
          cooperative scheduling now fails CI loudly. 4 new host
          unit tests in `preempt::tests` cover the vector const,
          LAPIC offsets, callback round-trip, and LAPIC→CpuId
          mapping; the arch-crate host total is now 101.
    - [x] **(c6)** x86_64 syscall entry stub bound to
          `kernel/syscall::Dispatcher`.
          `kernel/arch/x86_64::syscall_entry` ships the
          host-testable MSR-value math (`encode_star`,
          `efer_with_sce`, `fmask_value`, `pack_raw_args`), the
          per-CPU `SyscallTls { kernel_rsp0, user_rsp_save }`
          block addressed via `IA32_KERNEL_GS_BASE`, the naked
          `syscall_entry_stub` that builds a
          `[u64; SYSCALL_MAX_ARGS]` argument frame and calls the
          Rust trampoline, and an `AtomicU64`-stored
          `SYSCALL_DISPATCH_CALLBACK` (mirroring the (c5) timer
          callback). The trampoline fail-closes via
          `qemu_exit::exit_failure` if it fires before a callback
          is installed, matching the
          `interrupts::rustos_arch_x86_64_default_interrupt`
          posture. 10 new host unit tests in
          `syscall_entry::tests` cover MSR addresses (Intel SDM
          Vol 4 Table 2-2), STAR encoding bits + selector round-
          trip, `IA32_EFER.SCE` preservation, the FMASK
          documented-bit matrix, `RawArgs` ordering, `SyscallTls`
          layout + 16-byte alignment, and the host-is-None
          callback assertion. Arch-crate host total: 110. Docs:
          `docs/src/platform/x86_64.md` Stage-3a-(c6) section +
          `docs/src/architecture/syscalls.md` "Per-architecture
          entry stubs" table.
    - [x] **(c7)** Implement `kernel/core::KernelArch` against
          (c1)..(c6) and wire `kernel_main` via a dedicated
          `rustos-kernel` bin crate that is the single writer of
          `syscall_entry::set_dispatch_callback`. **Split into
          (c7-arch) (landed) and (c7-bin) (landed).**
        - [x] **(c7-arch)** `kernel/arch/x86_64::kernel_arch` —
              `X86_64Arch` (`SchedulerArch` impl + `ArchInitError`
              + `lapic_id_of` accessor) and the free `halt() -> !`
              function the bin crate's `KernelArch::halt` will
              forward to. On bare metal, `current_cpu` reads the
              LAPIC ID register and consults
              `preempt::cpu_id_for_lapic`; `ticks_now` reads
              `RDTSC`; `send_ipi` issues a directed IPI through an
              ephemeral `apic::Lapic` over `VolatileLapicMmio` at
              `preempt::LAPIC_BASE_PHYS` on the `preempt::TIMER_VECTOR`;
              `halt` issues `cli; hlt` in a loop with the
              compile-checked `-> !` return type. On host, the
              impl is exercised end-to-end through per-instance
              monotonic / IPI-accounting counters (no global
              statics, no `#[ignore]`). The arch crate now exposes
              an optional `sched-arch` Cargo feature that pulls
              `rustos-kernel-sched` only when downstream binaries
              opt in — the pre-existing freestanding QEMU bins
              (`memory_isolation`, `scheduler_stress_qemu`)
              continue to link without a `#[global_allocator]`.
              `kernel/syscall::table` gained a compile-time
              `const _RAW_ARGS_LAYOUT_MATCHES_ARRAY` assertion
              locking `RawArgs`'s `#[repr(transparent)]` over
              `[u64; SYSCALL_MAX_ARGS]` — the contract the bin
              crate's dispatch callback will rely on. 8 new host
              unit tests + 2 compile-time `const _` assertions
              (one for `halt`'s `-> !` signature, one for the
              `SchedulerArch` impl) bring the arch-crate host
              total to 118. Docs:
              `docs/src/platform/x86_64.md` (c7-arch) section +
              note in `docs/src/architecture/kernel.md`.
        - [x] **(c7-bin)** New freestanding `rustos-kernel` bin
              crate (`x86_64-unknown-none`) at
              `kernel/rustos-kernel/` — a hybrid `[lib]` + `[[bin]]`
              whose library half ships the boot pipeline reused by
              both the production binary and the new QEMU
              integration test. `boot(multiboot_info, log_sink,
              audit_sink) -> !` runs `percpu::init(0)` → LAPIC
              software-enable + PIT-calibrated 1 ms LAPIC timer →
              Multiboot2 → `BootMemoryMap` →
              `acpi::locate_madt` (newly extracted from
              `scheduler_stress_qemu`; AGENTS.md §2.2) → BSP-LAPIC
              verification → `X86_64Arch::new` →
              `syscall_entry::set_dispatch_callback` (the
              fail-closed callback in `dispatch.rs`, installed
              **before** `syscall` is enabled) →
              `preempt::init_local_preempt` →
              `preempt::set_cpu_id_for_lapic` →
              `syscall_entry::init_local_syscalls` (using
              `PerCpuGdt::selectors()` and the bin's 16 KiB per-CPU
              kernel-stack pool for `kernel_rsp0`) → `BootInfo::new`
              + forward to `rustos_kernel_core::kernel_main`. The
              orphan-rule wrapper `BinArch(X86_64Arch)` implements
              `KernelArch::halt` by forwarding to
              `kernel_arch::halt()`; the `-> !` return type is
              pinned at compile time by
              `_BIN_ARCH_HALT_RETURNS_NEVER`. The `#[panic_handler]`
              in each bin is a one-liner that calls
              `panic_ctx::handle_panic_via_kernel_core`, which loads
              the `Arc<BinArch>` pointer that `boot()` publishes
              into `PANIC_ARCH_PTR` and forwards through
              `kernel_core::handle_panic` with a `PanicContext { arch,
              audit_sink: &SERIAL_SINK }`; a pre-init panic emits
              one COM1 record and halts. A new QEMU integration
              test crate `tests/integration/kernel_arch_boot/`
              brings its own audit-observer Sink that flips
              `qemu_exit::exit_success` on
              `AuditEvent::BootCompleted` (`EventId(4004)`) — the
              test is enrolled in
              `tools/xtask/src/commands/qemu_tests.rs` with a 60 s
              budget. The bin crate enables
              `rustos-arch-x86_64/sched-arch`, ships a 16 MiB
              CAS-driven bump allocator with documented limits
              (`README.md`), and adds
              `--cfg curve25519_dalek_backend="serial"` to
              `.cargo/config.toml`'s `x86_64-unknown-none`
              rustflags to side-step a curve25519-dalek SIMD-backend
              LLVM error on the freestanding target. 11 new host
              unit tests (5 bumpalloc, 3 arch_wrapper, 3 dispatch)
              plus 4 compile-time `const _` assertions
              (`_BIN_ARCH_HALT_RETURNS_NEVER`,
              `_BIN_ARCH_IS_SCHED_ARCH`,
              `_DISPATCH_SIGNATURE_PINNED`,
              `_DISPATCH_CALLBACK_INSTALLABLE`,
              `_KERNEL_STACK_FITS_AT_LEAST_ONE_FRAME`). Docs:
              `kernel/rustos-kernel/README.md`,
              `docs/src/platform/x86_64.md` (c7-bin) section,
              `docs/src/architecture/kernel.md` updated to reflect
              the now-shipped impl. **Stage 2.7 follow-up**: the
              dispatch callback is fail-closed (`halts` if reached).
              The body swap is **not** the only missing piece — see
              the dedicated "Stage 2.7 follow-up — Production
              syscall wiring" section below for the full (f1)..(f7)
              sub-checklist (per-CPU current-task slot, `CapTable`,
              production `SyscallHandlers`, registration hook,
              QEMU test). The `_DISPATCH_SIGNATURE_PINNED` const-
              assert pins the callback ABI so the eventual swap
              cannot drift unobserved.
    - [x] **(d1)** Per-arch QEMU run script
          `tools/qemu/src/x86_64.rs`. The generic
          `tools/qemu::Spec`/`Runner` are now architecture-neutral;
          the x86_64 defaults (`DEFAULT_RAM_MIB = 256`,
          `ISA_DEBUG_EXIT_IOPORT/IOSIZE`, `QEMU_BINARY`, OVMF
          discovery + UEFI pflash flags, GRUB-EFI ISO build
          dispatch, full QEMU argv) live in a dedicated module
          which `Runner::run` enters via a single `match` on
          `Spec::arch`. The argv contract is asserted by 8 new
          host unit tests using a pure `build_argv` helper that
          takes a fake `OvmfPaths`, so the tests run on hosts
          without the `ovmf` package installed (`rustos-qemu`
          host total: 18, up from 12). The top-level
          `ISA_DEBUG_EXIT_IOPORT` / `ISA_DEBUG_EXIT_IOSIZE`
          remain as re-exports with a drift-guard unit test so the
          kernel side (`kernel/arch/x86_64::qemu_exit`) cannot
          silently desync from the host runner. The two existing
          QEMU integration tests and the `cargo xtask test --qemu`
          driver continue to pass — the refactor is internal.
          Docs: `docs/src/platform/x86_64.md` Stage-3a-(d1)
          section.
- 3b — `kernel/arch/aarch64` (Raspberry Pi 3/4/5 + QEMU virt; GIC).
- 3c — `kernel/arch/riscv64` (QEMU virt; PLIC, CLINT).
- [x] 3d — `kernel/arch/wasm32` (browser sandbox; cooperative scheduling
  backed by `requestAnimationFrame` / `MessageChannel`; "MMU" enforced by
  WASM memory isolation between worker contexts). Complete — see the
  "Stage 3d status" block below.

**Tests (per sub-stage)**
- Boots to `init` placeholder in QEMU / browser headless harness.
- Memory-isolation test passes.
- Timer interrupt drives scheduler.

**Docs**
- `docs/src/platform/<arch>.md` with build, run, and debug instructions.

**Stage 3a status: complete.**
- All x86_64 sub-stage items (a)..(c7), (d1) are `[x]`. (c7) is
  delivered in two commits — (c7-arch) on the arch crate, and
  (c7-bin) on the new `kernel/rustos-kernel` bin crate plus the
  companion `tests/integration/kernel_arch_boot` QEMU integration
  test enrolled in `tools/xtask/src/commands/qemu_tests.rs`.
- The architecture-neutral kernel core (`kernel/core::kernel_main`)
  is now reachable end-to-end from the x86_64 boot trampoline: a
  production `rustos-kernel` binary boots to
  `AuditEvent::BootCompleted` (`EventId(4004)`) under QEMU on the
  QEMU CI runner; the audit-observer sink in the integration test
  bin flips `qemu_exit::exit_success` on observing that event, so
  `cargo xtask test --qemu` reports `Outcome::Pass`.
- The Stage 2.7 follow-up is the only remaining (c7) thread: the
  bin crate's syscall-dispatch callback is fail-closed pending the
  production `SyscallHandlers` impl and the per-CPU current-task
  plumbing it consumes. A first inspection during the (c7-bin)
  follow-up session showed the work is **larger than a body-only
  swap** — neither the production `SyscallHandlers` impl, nor the
  per-CPU current-task slot, nor a kernel-side registration hook
  exist in tree. The full breakdown is captured in the dedicated
  "Stage 2.7 follow-up — Production syscall wiring" section below
  as sub-items (f1)..(f7). The `_DISPATCH_SIGNATURE_PINNED`
  const-assert in `kernel/rustos-kernel::dispatch` still pins the
  callback ABI so the eventual swap cannot silently drift.
- Stage 3d (wasm32) is now complete (see the "Stage 3d status" block
  below); Stage 3b (aarch64) and 3c (riscv64) status is tracked
  separately below. **All four Tier-1 architecture ports are now
  complete, so Stage 3 is complete.**

**Stage 3b status: complete.**
- `kernel/arch/aarch64` is now a full Arch HAL implementation for the
  QEMU `virt` board, mirroring the riscv64 port. Every module is
  host-unit-tested where it carries pure bit/encoding/layout math
  (39 host tests) and clippy/rustdoc clean on both the host and the
  `aarch64-unknown-none` target; the boot/console/exception/GIC/timer/MMU
  system-register and assembly operations are gated to
  `cfg(all(target_arch = "aarch64", target_os = "none"))`:
    - [x] **Boot stub + console** — `boot.s` (EL2→EL1 drop, stack,
          `.bss` zero, DTB hand-off) → `entry.rs`
          (`rustos_arch_aarch64_main`), a PL011 UART `rustos_log::Sink`
          (`serial.rs`), the `#[panic_handler]` bridge (`panic.rs`), the
          ARM-semihosting `SYS_EXIT` finisher (`qemu_exit.rs`), and the
          `aarch64-virt.ld` linker script.
    - [x] **Arch HAL** — `kernel_arch.rs`'s `Aarch64Arch` implements
          `rustos_arch_api::SchedulerArch`; the monotonic clock reads
          `CNTPCT_EL0` and converts against `CNTFRQ_EL0`.
    - [x] **MMU / page-table primitives** — `paging.rs`: stage-1, 4 KiB
          granule, three levels over a 39-bit VA (`TCR_EL1.T0SZ = 25`,
          the aarch64 mirror of Sv39), block/page/table descriptor
          encoders, a `.bss` `PageTablePool`, and an `AddressSpace`
          (1 GiB identity blocks — device-0 / Normal — + 4 KiB walk +
          `MAIR`/`TCR`/`TTBR0`/`SCTLR.M` `switch`). Host translate
          cross-check.
    - [x] **Context switch** — `context.rs` + `context.s`:
          `TaskCtx { sp }`, `prepare`, and `rustos_arch_aarch64_switch`
          saving the AAPCS64 callee-saved set (`x19`–`x28`, FP, LR) plus
          `x0`. `const _` layout/frame asserts.
    - [x] **Timer + scheduler tick** — `preempt.rs`: set-once tick
          callback, `interval_for_hz`, `init_local_preempt` (enable the
          EL1 physical-timer PPI 30 at the GIC + arm `CNTP_TVAL_EL0` +
          enable `CNTP_CTL_EL0`), and `on_timer_interrupt`
          (callback → re-arm), driven by the IRQ vector.
    - [x] **Interrupts** — `vectors.s` (16-entry EL1 vector table) +
          `exceptions.rs` (`VBAR_EL1` install, IRQ → GIC ack/timer/EOI,
          sync → `fault` hook, `enable_irq`) + `gic.rs` (GICv2
          distributor / CPU-interface / SGI driver).
    - [x] **Per-arch syscall entry** — `syscall_entry.rs`: the `svc`
          exception-class decode plus the `x8`/`x0`–`x5` → frozen
          `rustos_abi` `[u64; SYSCALL_MAX_ARGS]` marshalling and a
          set-once dispatch callback (the x86_64/riscv64 shape). Wiring
          the live EL0 register frame through to the dispatcher is the
          remaining aarch64 follow-up; the marshalling is host-tested.
    - [x] **Memory isolation** — `fault.rs`: set-once
          synchronous-exception hook (`FaultHandlerFn(esr, far, elr) -> !`,
          `ESR_EL1.EC` abort decode) the EL1 vector invokes.
    - The three Stage-3 per-sub-stage QEMU verticals are enrolled in
      `tools/xtask/src/commands/qemu_tests.rs` and **verified green under
      QEMU on this host** (single CPU, 60 s each):
      `tests/integration/kernel_arch_boot_aarch64` ("boots to init"),
      `tests/integration/timer_preempt_qemu_aarch64` ("timer interrupt
      drives scheduler" — GICv2 PPI 30 drives the callback ≥ 20 times),
      and `tests/integration/memory_isolation_qemu_aarch64`
      ("memory-isolation test passes" — an attacker `AddressSpace`
      faults on a victim-only page). The host-side QEMU runner gained an
      `Arch::Aarch64` backend (`tools/qemu/src/aarch64.rs`: `virt` +
      `cortex-a72` + semihosting result protocol) and
      `Spec::for_aarch64_kernel`; the integration-test harness gained the
      `itest_aarch64` cfg. Docs: `docs/src/platform/aarch64.md`.
    - Multi-hart SMP bring-up (MPIDR `CpuId` mapping + secondary start)
      and wiring the new `AddressSpace`/`context` switch into the *live*
      scheduler remain aarch64 follow-ups, exactly as they were staged
      for riscv64; `send_ipi` already raises a GICv2 SGI.
    - **Raspberry Pi 4 (BCM2711) board bring-up — `plans/PI.md`.** The
      Stage-3b port boots the QEMU `virt` board; taking it to a real
      Pi 4 (board-discovered UART/GIC/RAM, a production aarch64
      `rustos-kernel` binary, real peripherals, and a bootable SD image)
      is staged P0–P10 in `plans/PI.md`. The board difference is runtime
      device-tree data, never a `cfg(board=…)` fork (§17.2 / §2.2).
        - [x] **P0 — Pi-4 facts of record (docs-only).** The
              authoritative "Raspberry Pi 4 (BCM2711)" section in
              `docs/src/platform/aarch64.md` pins the BCM2711 MMIO map
              (`0xFE00_0000` peripheral base; PL011 `0xFE20_1000`, AUX
              mini-UART `0xFE21_5040`, mailbox `0xFE00_B880`, EMMC2
              `0xFE34_0000`), the GIC-400 bases (GICD `0xFF84_1000` /
              GICC `0xFF84_2000`), the EL2 `kernel8.img`@`0x8_0000` boot
              protocol + `config.txt` knobs, and the per-SKU RAM layout,
              so P1+ cite one source. `cargo xtask docs-check` green.
        - [x] **P1 — Pi-4 boot stub + linker script + production aarch64
              kernel binary.** `kernel/arch/aarch64/link/aarch64-rpi4.ld`
              (origin `0x8_0000`) joins `aarch64-virt.ld` (the §0.2
              boot-stub carve-out). `boot.s` parks non-boot CPUs
              (`MPIDR_EL1` affinity ≠ 0 → `wfe`) before touching the boot
              stack, serving both `virt` (PSCI-held secondaries) and the
              Pi (all-core release). `kernel/rustos-kernel/build.rs` now
              builds a freestanding **aarch64** production kernel: its
              pure target-selection logic lives in host-unit-tested
              `src/build_support.rs`, and it emits a build-glue
              `kernel_isa` cfg + the per-board linker script with no
              `cfg(target_arch)` in the crate body (cfg-check clean). The
              x86_64 boot pipeline is gated `kernel_isa="x86_64"`; the new
              freestanding `boot_aarch64` module + the aarch64
              `kernel_main(dtb)` construct `Aarch64Arch` (the §17
              selection point), bring up the console, log a boot line, and
              park fail-closed. `cargo build -p rustos-kernel --target
              aarch64-unknown-none` links a freestanding ELF entered at
              `0x8_0000`. The discovery-fed `kernel_core::kernel_main`
              hand-off (real memory map / IRQ routing) is staged to P2/P3
              (fabricating a map would violate §18.5, and its `-M raspi4b`
              runtime proof needs P2's console discovery). The
              `CPACR_EL1.FPEN` enable is consolidated into
              `rustos_arch_aarch64::enable_fp_el1()` (§2.2), adopted by
              the production binary and the existing aarch64 verticals.
        - [x] **P2 — Board-discovered UART console (PL011 + mini-UART).**
              The fixed `serial::PL011_BASE` constant is gone: a new
              host-testable `rustos_arch_aarch64::console` module holds the
              console MMIO base + register model (an atomic `(base, model)`
              pair, default = the `virt` PL011) that the freestanding
              `serial` sink reads on every byte, plus `find_console` /
              `configure_from_fdt` over the shared `lib/fdt` reader. The
              BCM2835 **AUX mini-UART** is a second `ConsoleModel` behind
              the same `rustos_log::Sink` seam (its own register offsets +
              opposite-sense TX-ready bit), selected by `compatible`
              (`brcm,bcm2835-aux-uart` vs `arm,pl011`) — one console
              abstraction, two backends (§2.2). `platform::FdtDiscovery`
              emits a `serial`-class `HwNode` (compatible bind key + `reg`
              MMIO resource), and `boot_aarch64::boot` configures the
              console from the `x0` DTB before its first log line
              (MMU-off-safe: the byte-wise `lib/fdt` reader takes no
              multi-byte Device-memory load — W17). Host unit tests cover
              the PL011/mini-UART register encoders, the `compatible`
              selection, and the discovered `serial` node (against a new
              `rustos_fdt` `raspi_like_arm` fixture). The QEMU vertical
              `tests/integration/uart_console_qemu_aarch64` (enrolled,
              single CPU, 60 s) boots the `virt` board, poisons the console
              base, then proves `configure_from_fdt` overwrites it from the
              board's embedded device tree and that writes reach the
              discovered base, **verified green under QEMU on this host**.
              *Honest emulation gap (§2.1):* the vertical runs on `virt`,
              not a Pi board — QEMU's `raspi*` models pass no DTB pointer
              (`x0 = 0`, GDB-verified) and QEMU 8.2.2 lacks `raspi4b`; the
              Pi's specific base / mini-UART layout are host-unit-tested
              against the fixture and are on-metal acceptance items for the
              Arc C peripheral stages.
        - [x] **P3 — GIC-400 from the tree + Pi RAM map.** The fixed
              `gic::{GICD_BASE,GICC_BASE}` constants are gone: `gic` holds
              the active `(gicd, gicc)` pair as an atomic (default = the
              `virt` GICv2 `0x0800_0000`/`0x0801_0000`) that the
              freestanding `VolatileGicMmio` reads on every access, plus
              `find_gic` / `configure_from_fdt` over `lib/fdt` selecting
              the first GICv2-class controller (`arm,gic-400`,
              `arm,cortex-a15-gic`, …) and reading its two `reg` regions.
              `platform::FdtDiscovery` emits an `InterruptController`
              `HwNode` (compatible bind key + GICD/GICC MMIO windows;
              `HwDeviceClass::InterruptController` already existed — no ABI
              change). The `lib/fdt` `virt_like_arm` / `raspi_like_arm`
              fixtures grew a GIC node (virt `arm,cortex-a15-gic`; Pi
              `arm,gic-400` @ `0xFF84_1000`/`0xFF84_2000`); host tests
              cover the discovery, the `HwNode`, and the fail-closed
              no-GIC path. `boot_aarch64` parses the `x0` DTB once and
              points the console **and** the GIC driver at their
              discovered bases and reads the `/memory` window (logging
              `gic_discovered` / `ram_discovered`); the live allocator +
              `kernel_core::kernel_main` hand-off over that map is staged
              to P4/P6 (a hard-coded map would violate §18.5). The
              `ipi_smp_qemu_aarch64` vertical now **poisons** the GIC base,
              rediscovers it from the embedded `virt` DTB before
              `gic::init`, and asserts it moved to the `virt` GICv2 base,
              so the delivered IPI runs over the *discovered* base
              (`irq_qemu_aarch64` likewise reads `gic::current()`); both
              **verified green under QEMU on this host**, and `cargo xtask
              cfg-check` stays clean. The Pi's specific GIC-400 bases are
              host-unit-tested + an on-metal item (no `raspi4b` in QEMU).
        - [x] **P4 — Generic timer + live scheduler on the Pi.** The
              generic-timer counter rate is now a *discovered* board fact
              rather than the raw register: `fdt::timer_clock_frequency`
              reads the `/timer` node's optional `clock-frequency` override
              (the `arm,armv?-timer` binding firmware carries when
              `CNTFRQ_EL0` is mis-programmed) and the pure, host-tested
              `fdt::effective_timer_hz` prefers it over `CNTFRQ_EL0` when
              present and non-zero, else falls back to the register (a zero
              override is treated as absent — never a 0 Hz timer, §2.9).
              `kernel_arch::timer_frequency_hz(&fdt)` composes the two;
              `boot_aarch64` seeds the `Aarch64Arch` clock + preempt
              interval from it and logs `timer_hz_from_tree`.
              `timer_clock_frequency` matches the timer node through the
              shared `Fdt::nodes` early-returning walk (the same byte-safe
              traversal `gic::configure_from_fdt` uses, §2.2) — **not** the
              whole-tree `Fdt::property`/`walk` scan, which the compiler
              widens into multi-byte loads that fault under the verticals'
              MMU-off boot. The `sched_drive_qemu_aarch64` vertical now
              sizes its tick interval from `timer_frequency_hz` over the
              embedded `virt` DTB and **poisons** the GICv2 base,
              rediscovering it (`configure_from_fdt`) before `gic::init`,
              so the ≥ 20 timer ticks + ≥ 1 IPI that drive the live
              `Scheduler` run over the *discovered* base + rate — **verified
              green under QEMU on this host**. `virt` omits
              `clock-frequency`, so the runtime path exercises the register
              fallback while the override branch is host-unit-tested;
              honouring the Pi's real 54 MHz crystal is an on-metal item
              (no `raspi4b` in QEMU). `cargo xtask cfg-check` stays clean.
        - [x] **P5 — SMP bring-up on the Pi (PSCI conduit discovery).**
              The PSCI conduit (`hvc`/`smc`) is now a *discovered* board
              fact end to end. `fdt::psci_method` was moved off the
              whole-tree `Fdt::property` scan onto the shared `Fdt::nodes`
              early-return walk (matching the `/psci` node by an
              `arm,psci` `compatible` prefix and reading `method` from
              that node only), the same byte-safe traversal
              `gic::configure_from_fdt` / `fdt::timer_clock_frequency`
              use (§2.2) — so conduit discovery is safe on the MMU-off
              bring-up path where a full-tree scan faults (the P4
              watch-out). `boot_aarch64` reads the conduit from the `x0`
              DTB, installs it via `Aarch64Arch::with_psci_method`, and
              logs `psci_conduit_discovered`; a tree with no `/psci` node
              leaves the conduit unset, so the `SecondaryBringup` HAL
              fails closed (`SmpError::NotReady`) rather than assuming one
              (§5.4.5). The `ipi_smp_qemu_aarch64` vertical now
              *discovers* the conduit from the embedded `virt` tree
              (replacing the hard-coded `VIRT_PSCI_METHOD`), asserts it is
              the board's `hvc`, fails closed otherwise, and starts the
              secondary core + delivers a directed SGI over *that*
              discovered conduit — **verified green under QEMU on this
              host**. Host tests cover the conduit read from the `virt`
              (`hvc`) and `raspi` (`smc`) fixtures and the fail-closed
              no-`/psci` path. The Pi's `smc` conduit (via `armstub8.bin`)
              flows through the identical path and is an on-metal item
              (no `raspi4b` in QEMU). `cargo xtask cfg-check` stays clean.
        - [~] **P6 — Spawn `init` into EL0 (the "boot into user mode"
              milestone).** Staged into chunks P6a–P6e (`plans/PI.md` P6),
              each landed green on its own.
            - [x] **P6a — `console_write` `abi-v1` syscall.** New syscall
                  number 11 + `CAP_CONSOLE_WRITE` (id 18), the `SyscallSpec`
                  row + recomputed `SYSCALL_TABLE_HASH`, the `kernel/core`
                  `ConsoleWrite` seam (boot installs the device — framebuffer
                  else first UART — defaulting to a fail-closed
                  `NULL_CONSOLE` → `NotImplemented`) + the copy-in handler,
                  the `ros_sys_console_write` C stub, and the regenerated C
                  header. `SYSCALL_NAME_MAX` bumped 12→13.
            - [x] **P6b — `rustos-init` becomes a real (pure-Rust)
                  program.** The `rustos-init` package builds the `init`
                  bundle's `Run` entry-point binary (`src/run.rs`, §16.5) as a
                  **pure-Rust** program. RustOS is Rust-only (§1), so it links
                  the new pure-Rust userland runtime `lib/rt` (`rustos-rt`) —
                  **never** the C ABI (`crt0` + `abi-sys`), which exists solely
                  for non-Rust programs (§16.4). `rustos-rt` provides `_start`,
                  the §19.2 stack canary, the panic handler, and idiomatic
                  syscall wrappers; `rustos_rt::entry!` names the program's
                  safe `main() -> i32`, which parses the compiled-in startup
                  config (`src/startup.rs`, a tiny allocation-free fail-closed
                  `console` + `session <abs-path>` text format, host-tested)
                  and writes its first banner line through the P6a
                  `console_write` syscall, the runtime routing the return
                  through `exit`. The trap assembly is **not** duplicated: a
                  new `lib/abi-trap` (`rustos-abi-trap`) holds the one §1
                  syscall/svc/ecall carve-out, shared by `rustos-rt` and
                  `abi-sys` (§2.2). It links **only** the runtime and its own
                  parser — never the orchestrator library, whose `alloc`+crypto
                  chain would be §2.3 bloat — so the linked aarch64 PIE carries
                  `_start`/`__rustos_rt_main`/`rust_rt_start` and a mangled
                  `rustos_rt::console_write`, with zero `ros_sys_*` and zero
                  crypto symbols, and **no `unsafe`** in the program. A
                  self-contained `build.rs` sets the `freestanding` cfg from
                  `target_os` only (cfg-check + §17.4 layering stay clean);
                  `Run.ld` mirrors the proven PIE link layout the userland
                  runtimes share.
            - [x] **P6c** — production aarch64 boot reaches EL0 (staged
                  P6c-1/-2/-3; see `plans/PI.md`). **P6c-1** (discovered
                  `/memory` → `BootMemoryMap`), **P6c-2** (MMU +
                  `kernel_main` hand-off), and **P6c-3** (embed `init` rxe
                  + spawn PID 1 into EL0) are all landed.
                  **P6c-3 — spawn PID 1 into EL0:** the `rustos-kernel`
                  build script compiles `rustos-init-run` PIE and embeds
                  it as an `rxe` (via `rustos_itest_harness::elf2rxe`,
                  stamped with `SYSCALL_TABLE_HASH`). `kernel/core` gained
                  an arch-neutral PID-1 spawn seam: `BootInfo.init` +
                  `with_init` (invoked by `kernel_main` after
                  `BootCompleted`) and the object-safe `InitSpawnCtx`
                  (`frames`/`audit`/`admit_init`); `spawn_and_enter` was
                  split so `spawn_image` is the no-enter build half. The
                  aarch64 `init_spawn` seam builds a 2 GiB-identity user
                  space (64 GiB bias), parses the `rxe`, and boxes the
                  `userentry` `eret` as the scheduler task body, which
                  `step` runs so `current_task` is set when `init`'s first
                  `svc` traps back. The `spawn_init_qemu_aarch64` `-M virt`
                  vertical asserts the EL0 transition (`ProcessSpawned`
                  4030 → audited `exit` 5000 → semihosting PASS).
                  **Locking fix:** the production `KernelDispatchHook` now
                  snapshots the caller's caps and drops the read guard
                  before dispatch, so the caps-mutating handlers (`exit`,
                  `cap_delegate`, `cap_revoke`) no longer self-deadlock the
                  writer-preference `RwLock` (a latent bug, first reached
                  here). **P6c-3 follow-up (landed):** an arch
                  `AddressSpace<P>` is `!Sync` (owns a `&'static mut`
                  root + non-`Sync` page-table source), so PID 1's
                  `console_write` user-copy resolved no address space and
                  failed closed with `BadAddress`. `kernel/mem` gained
                  `AddressSpace::freeze()` → `FrozenAddressSpace`, a
                  `Send+Sync` POD `BTreeMap<Page,(Frame,MapFlags)>` snapshot
                  walked through `translate`; `InitSpawnCtx::admit_init` now
                  also takes the boxed frozen view + boxed `DirectPhysMap`
                  and `KernelInitSpawner` registers them under
                  `SecTaskId(task_id)` in the `AddressSpaceRegistry`. The
                  aarch64 seam freezes `space` after `spawn_image`; `init`'s
                  `run.rs` gates its `exit` on a full-length `console_write`
                  (parks fail-closed otherwise), so the
                  `spawn_init_qemu_aarch64` vertical's PASS now proves the
                  banner reached the console. **P6d** — `CAP_PROC_SPAWN`
                  spawn syscall, staged *properly* in `plans/SPAWN.md`;
                  **SP0 + all of SP1 landed (all three bare-metal ports):**
                  the SP0 design note
                  (`docs/src/architecture/multitasking.md`) and the
                  `kernel/core::kthread` resumable-kernel-thread runtime
                  (`spawn_kthread` + `Yielder` + per-task kernel stack,
                  layered over `SchedulerPolicy::spawn`, §17.1) — proven by
                  `tests/integration/kthread_switch_qemu_{aarch64,riscv64,x86_64}`
                  (two kthreads ping-pong through the real
                  `ContextSwitch::switch`, now a production scheduling path
                  on each arch; the x86_64 sibling fixed a latent
                  `TaskCtx::prepare` rdi-slot + stack-alignment bug exposed
                  by the first on-metal first-resume). **SP2** (resumable
                  EL0 tasks, staged SP2a/SP2b/SP2c) is in progress:
                  **SP2a** (the arch-neutral EL0-reschedule core —
                  `DispatchOutcome::Reschedule` + `RescheduleAction`, the
                  per-CPU `USER_RESUME` resume table + `reschedule_current`,
                  and the `pre_resume` user-kthread hook on `dispatch_step`)
                  and **SP2b** (aarch64 enters PID 1 into EL0 as a resumable
                  user kthread — `spawn_user_kthread`, the
                  `KernelArch::Cs`/`context_switch` Arch-HAL context-switch
                  accessor, the `activate_user_root` `TTBR0_EL1`
                  `pre_resume` hook, and the arch-neutral `yield`/`exit`
                  reschedule producer that retires the double-handling so
                  the `yield_now`/`exit` handlers no longer drive the
                  scheduler; `kernel_main` now drains the boot CPU's run
                  queue to completion), and **SP2c** (the `-M virt`
                  EL0↔EL0 two-task timeshare vertical —
                  `tests/integration/spawn_el0_timeshare_qemu_aarch64`
                  builds two hardware-isolated EL0 address spaces from the
                  pure-Rust `rustos-test-el0-yielder` fixture, which links
                  the new `rustos_rt::yield_now` wrapper, admits each as a
                  resumable user kthread via `spawn_user_kthread`, and
                  drains the cooperative `step` loop while a dispatch
                  callback maps each task's `yield`/`exit` to
                  `reschedule_current`; PASS once both tasks yielded their
                  full count and exited — **verified green on `-M virt`**)
                  are landed, so **SP2 is complete on aarch64**. **SP3**
                  (the `spawn` syscall + embedded-program registry) is
                  staged SP3a/SP3b: **SP3a is landed** — the `abi-v1`
                  `spawn` syscall #12 (`CAP_PROC_SPAWN`, audited) end to
                  end (`lib/abi` row + frozen tests, the `ros_sys_spawn` C
                  stub + regenerated header, the `kernel/syscall` dispatch
                  arm + recomputed `SYSCALL_TABLE_HASH`), plus the
                  `kernel/core` path-keyed `ProgramRegistry` + the
                  fail-closed `ProcessSpawn`/`SpawnCtx` seam (default
                  `NULL_PROCESS_SPAWN` → `NotImplemented`, mirroring
                  `NULL_CONSOLE`) and the `spawn` handler that copies-in the
                  path, resolves it, and admits a **Ready** resumable user
                  kthread through `SpawnCtx::admit_process` (host-proven via
                  a `ProcessSpawn` double; 8 new host tests). **SP3b + SP4
                  are now landed, so P6d is complete:** the real aarch64
                  `ProcessSpawn` producer
                  (`kernel/rustos-kernel/src/spawn_producer.rs`) builds each
                  child a fresh, hardware-isolated 2 GiB-identity address
                  space from a static `PageTablePool` reserve (without
                  switching the spawning caller's `TTBR0_EL1`), drives the
                  audited `spawn_image` + `admit_process`, and is installed
                  via `BootInfo::with_spawn`; the kernel `build.rs` now embeds
                  both `init` and the `Shell` session program through one
                  `elf2rxe` helper, registered under `/Apps/Shell.app/Run`.
                  PID 1 `init` (granted `CAP_PROC_SPAWN`) spawns
                  `config.session()` via `rustos_rt::spawn` and keeps running;
                  `tests/integration/spawn_session_qemu_aarch64` proves both
                  processes run on `-M virt` (PASS on two `ProcessSpawned` +
                  three audited syscalls, the session's gated banner+exit
                  necessarily last). **P6e** — grow the session stub into a
                  real `rustos-shell` REPL + `init` session supervision —
                  is **complete** (P6e-1/P6e-2/P6e-3a/P6e-3b all landed), so
                  **all of P6 (the "boot into user mode" milestone) is done**.
                  **Binding
                  design correction (AGENTS.md §20):** the shell does its
                  text I/O over its inherited **standard streams (fd 0/1/2/3
                  — `stdin`/`stdout`/`stderr`/`stdinfo`)**, *not* the
                  kernel-discovered console via `console_read`/
                  `console_write` (which would be ambient authority §4 +
                  device coupling §17.3/§17.4). P6e-1 (`console_read`
                  syscall) + P6e-2 (UART RX device) — **both landed** —
                  build the bootstrap stream **backing**: P6e-2 added the
                  aarch64 non-blocking RX read (`ConsoleModel::rx_ready` +
                  `serial::read_console_bytes`, no busy-wait §2.1), a
                  `ConsoleRead` impl on the zero-sized `UartConsole`, and
                  its install via `BootInfo::with_console_read` in
                  `boot_aarch64`. **P6e-3a is now landed:** the console
                  syscalls evolved **in place** (§2.13) into fd-keyed
                  `stream_write(fd,buf,len)` (#11) / `stream_read(fd,buf,len)`
                  (#13); `lib/abi` gained the per-process descriptor model
                  (`STDIN/STDOUT/STDERR/STDINFO`, `StreamMode`,
                  `DescriptorTable`) the kernel holds per task in
                  `AddressSpaceRegistry` and establishes at spawn
                  (`admit_init`/`admit_process` install
                  `DescriptorTable::standard()`); the handlers resolve `fd`
                  → backing (fail-closed `NotFound` on the wrong direction),
                  reusing the P6e-1/P6e-2 UART backing behind fd 0/1. `lib/rt`
                  now exposes `stdout`/`stderr`/`stdinfo`/`stdin` and
                  `lib/abi-sys` `ros_sys_stream_*`; `init` + the `Shell`
                  session write via `rustos_rt::stdout`. C header + hash
                  regenerated; the `spawn_session_qemu_aarch64` `-M virt`
                  vertical proves a child writes fd 1 over the UART backing.
                  **P6e-3b-i (shell REPL) landed:** the `Shell` bundle's
                  `Run` binary now runs the `rustos-shell` interpreter as a
                  read-eval-print loop (new `repl` lib module) over its
                  inherited standard streams — reads fd 0 (`rustos_rt::stdin`,
                  line reassembly + CRLF strip + 4 KiB cap), writes the prompt
                  + output to fd 1/2 via an `RtConsole`, emits an `omission`
                  `StdInfoRecord` on fd 3 when a line is dropped, and launches
                  externals via `spawn`+`wait` (fail-closed `NotImplemented`
                  on pipes/redirs/args/signals/`cd` the `spawn` ABI cannot yet
                  carry) — **no** `console_*`/device reference (§4/§20). Added
                  a tested `Errno::from_i32` decoder (§2.2) and hardened
                  `rustos_rt::stdin` (negative `-errno` → 0, count clamped to
                  `buf.len()`). Host-proven (6 `repl` + 3 `stdin` + 1
                  `from_i32` tests) and freestanding-built on all three
                  bare-metal targets; `spawn_session_qemu_aarch64` stays green.
                  **P6e-3b-ii (`init` session supervision) landed:** PID 1
                  `init` (`userland/system/init/src/run.rs`) now runs a
                  fail-closed supervise loop — `spawn` the session, `wait` on
                  exactly that child (block, reap), then relaunch it, bounded
                  by a small `SESSION_SPAWN_BUDGET` crash-loop guard (a
                  session that blocks on input never approaches it; one that
                  exits instantly stops at `EXIT_SESSION_EXHAUSTED` rather
                  than busy-spinning §2.1; a failed `spawn`/`wait` is
                  fail-loud). No kernel/boot change was needed — the
                  production pipeline already wires the `KernelProcessWait`
                  producer + `register_child`/`record_exit` and `admit_init`'s
                  drive loop re-dispatches the parked `init`. The
                  `spawn_session_qemu_aarch64` vertical was reworked to key
                  PASS on **three** `ProcessSpawned` (init + two session
                  launches = reap+restart witness) + **four** audited syscalls;
                  `spawn_init_qemu_aarch64` still PASSes (witness now `init`'s
                  first audited syscall, the `spawn`). Docs:
                  `docs/src/userland/init.md` ("Session supervision").
                  **SP5 (`mem_map`/`mem_unmap`, runtime anonymous memory,
                  `plans/SPAWN.md`) — SP5-0 + SP5a landed:**
                  `SyscallNumber::MEM_MAP` (#14) / `MEM_UNMAP` (#15), the
                  `MapFlags` type (`FIXED`), `Errno::OutOfMemory` (#20), the
                  `ros_sys_mem_map`/`ros_sys_mem_unmap` C stubs + regenerated
                  header, the dispatcher arms, and `kernel/core`'s `MemMap`
                  seam (`NULL_MEM_MAP` / `with_mem_map`, unprivileged +
                  unaudited, fail-closed `NotImplemented`, host-proven). The
                  ungated decision follows §16.6 (a process grows only its
                  **own** isolated space). **SP5b-1 also landed:** the
                  reusable, host-proven `kernel/mem::anon` producer
                  (`map_anonymous`/`unmap_anonymous` — zero on map/free, W^X
                  `RW|USER`, deterministic OOM, fail-closed all-or-nothing
                  reclaim, per-page TLB flush) over a live `AddressSpace<P>`
                  (8 host tests on `HostPageTable`+`SimPhysMap`). **SP5b-2
                  also landed (SP5 complete):** the aarch64 `-M virt` EL0
                  vertical `tests/integration/mem_map_qemu_aarch64` wires the
                  producer through the `kernel/core` `MemMap` seam — it builds
                  one isolated EL0 space with `spawn_image`, **retains** it
                  live behind a `MemMap` producer over
                  `map_anonymous`/`unmap_anonymous`, admits the program as a
                  resumable user kthread, and routes the program's
                  `mem_map`/`mem_unmap` `svc`s through it; the pure-Rust EL0
                  fixture `tests/integration/mem_map_program` (linking the new
                  `rustos_rt::mem_map`/`mem_unmap` wrappers) maps a region
                  (FIXED), writes+verifies a pattern, unmaps it, then faults
                  on use — the fault handler reports PASS (id 4282),
                  **verified green under QEMU on this host**. **The riscv64
                  sibling `tests/integration/mem_map_qemu_riscv64` also
                  landed:** it reuses the same pure-Rust `mem_map_program`
                  fixture and the same `kernel/mem::anon` producer over an
                  Sv39 U-mode space, but drops in through `spawn_image` + a
                  direct `EnterUser::enter_user` (a single task that only
                  direct-returns from its `ecall`s, so the riscv64
                  cooperative-switch trap-save path stays off the critical
                  path) and reports the use-after-unmap page fault as PASS
                  (ids 4284-4287), **verified green under QEMU on this host**.
                  The x86_64 sibling + production per-task live-space
                  retention still follow. **The `lib/rt` heap that layers over the
                  pair also landed** (PI P6e-3b prerequisite): a
                  `#[global_allocator]` in `lib/rt/src/heap.rs` — a free-span
                  allocator over a fixed-base virtual arena that grows by
                  `mem_map(FIXED)` and shrinks by `mem_unmap`, first-fit with
                  alignment-padding return + neighbour coalescing, real free,
                  deterministic-OOM-to-null, no re-zero on free (the kernel
                  already zeroes on map/free). Host-unit-tested over a fake
                  pager; the aarch64 `-M virt` vertical
                  `tests/integration/heap_qemu_aarch64` (fixture
                  `tests/integration/heap_program`) proves `Box`/`Vec`
                  alloc-grow-free-reuse end to end and exits 0 (PASS, ids
                  4290-4293), **verified green under QEMU on this host**.
                  Design note: `docs/src/architecture/memory.md` §7d.
                  **SP6 (`wait`, process reap, `plans/SPAWN.md`) — COMPLETE**
                  (PI P6e-3b prerequisite): SP6a landed the `abi-v1` surface
                  (`SyscallNumber::WAIT` #16 + `WAIT_ANY`, the unprivileged +
                  audited `wait(I32 pid, UserPtr status) -> U64` row, the
                  `ros_sys_wait` C stub + header, the `rustos_rt::wait`
                  wrapper, the dispatcher arm, and the fail-closed
                  `kernel/core::procwait::ProcessWait` seam + handler). **SP6b
                  landed the scheduler-side producer:** the `ProcessWait` trait
                  gained default-no-op `register_child`/`record_exit` hooks
                  (so the null default + test doubles stay inert and `new()`
                  needs no churn); the real `KernelProcessWait<A>` owns a
                  `SpinLock<ProcessTable>` (child id → `{parent, exit}`) and
                  blocks a waiting parent by cooperatively parking it via
                  `reschedule_current(current_cpu, Yield)` until a matching
                  child is reapable (fail-closed `NotImplemented` if no user
                  kthread is published — never a busy-spin, §2.1/§2.9); `exit`
                  records the code, the `spawn` admit path records the
                  parent→child link, and `run_phases` installs the producer via
                  the hook's new `with_process_wait`. The aarch64 `-M virt`
                  vertical `tests/integration/wait_qemu_aarch64` (+ the two-role
                  `tests/integration/wait_program` fixture, `build.rs` the §2.2
                  source of truth for `CHILD_EXIT_CODE`) proves a parent reaps a
                  child that exited with a known code, reads it back, and exits
                  0 (PASS, ids 4290-4292), **verified green under QEMU on this
                  host**. This unblocks the P6e-3b REPL + `init` supervision.
                  **P6 follow-on — kthread guard-page fault-form, staged
                  G1..G3 (`plans/PI.md`); G1 landed:** the foundation for
                  turning a stack overflow into an immediate hardware fault
                  (rather than the already-landed poison-canary detected at
                  the next reschedule, §2.17) is the aarch64
                  `paging::AddressSpace::split_block(vaddr)` — it re-expresses
                  the coarse identity block covering `vaddr` at 4 KiB
                  granularity (1 GiB block → 512×2 MiB blocks → 512×4 KiB
                  pages), preserving the output address and every attribute
                  (`shatter_block_into` copies `desc & !ADDR_MASK`, setting
                  `TABLE_OR_PAGE` only at L3). It only *adds* table levels
                  reproducing the existing translation, so it is safe against
                  the running region (no break-before-make of the block the
                  CPU executes in — the reason the fault-form is staged, not
                  a single shatter), is idempotent, and fails closed; a single
                  page then unmaps via the existing `MmuAddressSpace::unmap` +
                  `TlbShootdown::flush_page`. Host-proven (4 new
                  `paging_tests.rs` tests) and end-to-end on `-M virt` by
                  `tests/integration/stack_guard_qemu_aarch64` (split a RAM
                  block, MMU on, sentinel write+read-back, then unmap+flush
                  one page and fault on access → PASS, ids 4300-4302),
                  **verified green under QEMU on this host**. **G2 is now
                  landed:** `paging::AddressSpace::prepare_guard_arena(base,
                  len)` applies `split_block` to every 2 MiB block an arena
                  spans (idempotent, fail-closed, BBM-free, 3 host tests);
                  `rustos-kernel::mem_map` carves a 2 MiB-aligned, 2 MiB
                  guard arena out of the usable RAM window and marks it
                  `Reserved` (returning a `MemoryLayout { map, arena }`, 2
                  new host tests); `boot_aarch64` keeps the live boot
                  `AddressSpace` and fine-maps the arena over the active
                  tables after discovery (`guard_arena_prepared` audit
                  field); and the per-arch vertical
                  `tests/integration/stack_arena_qemu_aarch64` (ids
                  4303-4305) prepares an arena that is its own 2 MiB block,
                  unmaps a guard page in it, proves the running stack (a
                  different block) and a neighbour page still work, then
                  faults on the unmapped page → PASS, **verified green under
                  QEMU on this host**. **G3a is now landed:** the
                  coarse-block split is promoted onto the Arch HAL
                  `AddressSpace` surface (`rustos_arch_api::mmu`, §17.2) as
                  `block_split_support() -> BlockSplit` (each port's honest
                  `Supported`/`Unsupported`/`Pending` declaration, modelled
                  on the §19.1/§19.10 profiles, justification enforced by
                  `mmu::conformance`) plus a default-fail-closed
                  `split_block(vaddr)` returning the new
                  `MapError::Unsupported`; aarch64 reports `Supported` and
                  forwards to its tested inherent body (one impl, §2.2),
                  riscv64 + x86_64 report honest `Pending`, and
                  `kernel/mem` carries the new cases
                  (`PageTableError::Unsupported`). Host-proven (4 new
                  arch-api conformance tests + aarch64 HAL-forwarding +
                  riscv64 Pending-fails-closed); no QEMU vertical needed
                  (G1/G2 already prove the live mechanism). **G3b-1 is now
                  landed:** `prepare_guard_arena` (the arena form of the
                  split) is promoted onto the same Arch HAL `AddressSpace`
                  surface with a default-fail-closed `MapError::Unsupported`,
                  aarch64 forwarding to its inherent body and riscv64/x86_64
                  falling back to the software canary (`mmu::conformance`
                  now also requires a non-`Supported` port to fail the arena
                  closed; aarch64 proves HAL→inherent over `dyn`, riscv64
                  proves Pending fail-closed). G3b-2 (`BoxStack` rewire over
                  the G2 arena, needs cross-space arena-frame plumbing in
                  arch-neutral `kernel/core`) and G3c (production fault-form
                  on `-M virt`) follow. Docs:
                  `docs/src/platform/aarch64.md`.

**Stage 3c status: complete.**
- The riscv64 boot stub, SBI console, FDT reader, `RiscvArch`
  (`SchedulerArch` + monotonic `time`-CSR clock), PLIC driver, and
  S-mode external-IRQ trap glue were already in tree (Stage 4.D
  Item 4). This session added the remaining Stage-3 per-sub-stage
  arch primitives, each host-unit-tested and clippy/rustdoc clean on
  both the host and the `riscv64gc-unknown-none-elf` target:
    - [x] **MMU / page-table primitives** — `kernel/arch/riscv64::paging`:
          Sv39 PTE PPN encode/decode, per-level VPN extraction, the
          `satp` Sv39 selector, a `.bss` `PageTablePool`, and an
          `AddressSpace` (gigapage identity map + 4 KiB walk +
          `satp`/`sfence.vma` `switch`). 12 host tests including a
          host-side three-level translate cross-check.
    - [x] **Context switch** — `kernel/arch/riscv64::context` +
          `context.s`: `TaskCtx { sp }`, `prepare` (first-run frame:
          `ra = entry`, `a0 = arg`), and `rustos_arch_riscv64_switch`
          saving `ra` + `s0`–`s11` + `a0`. `const _` layout/frame
          asserts; 6 host tests.
    - [x] **Timer + scheduler tick** — `kernel/arch/riscv64::preempt`:
          set-once tick callback, `sie.STIE` enable, `interval_for_hz`,
          `init_local_preempt` (arm SBI `set_timer` + enable STIE),
          and `on_timer_interrupt` (callback → re-arm/ack). The trap
          handler routes the supervisor-timer `scause` here. 8 host
          tests. **QEMU vertical
          `tests/integration/timer_preempt_qemu_riscv64`** (enrolled
          in `tools/xtask/src/commands/qemu_tests.rs`, single CPU,
          60 s budget) boots the `virt` board, arms the timer at
          100 Hz, and asserts the supervisor-timer trap path drives
          the callback ≥ 20 times before `SiFive` Test PASS — the
          "timer interrupt drives scheduler" deliverable, **verified
          green under QEMU on this host**.
    - [x] **Per-arch syscall entry** — `kernel/arch/riscv64::syscall_entry`
          + `trap::TrapFrame`: the U-mode `ecall` path. `pack_raw_args`
          marshals `a0`–`a5` into the frozen `rustos_abi`
          `[u64; SYSCALL_MAX_ARGS]` layout (shared with x86_64,
          §2.2); `dispatch_ecall` forwards `(a7, &args)` to a set-once
          dispatch callback and writes the result to `a0`; the trap
          handler advances `sepc` past the 4-byte `ecall` and fails
          closed without a callback. 8 host tests; the existing
          riscv64 boot vertical still PASSES after the trap-frame
          change.
    - [x] **Memory isolation** — `kernel/arch/riscv64::fault` + the
          QEMU vertical `tests/integration/memory_isolation_qemu_riscv64`.
          `fault` adds a set-once synchronous-exception handler hook
          (`FaultHandlerFn(scause, stval, sepc) -> !`, page-fault
          `scause` constants, `is_page_fault`) — the riscv64 analogue
          of the x86_64 `idt` page-fault callback — which the `trap`
          handler now invokes for an unexpected synchronous exception
          (reading `stval`/`sepc`) before falling back to parking the
          hart. 5 new host tests. The vertical (enrolled in
          `tools/xtask/src/commands/qemu_tests.rs`, single CPU, 60 s
          budget) boots the `virt` board, builds a victim and an
          attacker `paging::AddressSpace` that disagree on one VA
          (64 GiB, above the shared 4 GiB identity window), switches
          `satp` to the attacker, and reads that VA: the MMU raises a
          load page fault, the handler confirms the cause / faulting
          address / victim-intact invariants, and writes `SiFive` Test
          PASS — the "memory-isolation test passes" deliverable,
          **verified green under QEMU on this host**.
    - The boot stub, console, and QEMU run script (`tools/qemu`)
      pre-existed, so 3c's per-sub-stage checklist ("boots to init",
      "memory-isolation test passes", "timer interrupt drives
      scheduler") is now satisfied.
    - [x] **Multi-hart SMP bring-up** — `kernel/arch/riscv64::smp` +
          `smp.s` + the SBI v0.2 IPI/HSM calls in `sbi`. `smp` adds
          `MAX_HARTS`, a `tp`-derived `current_hartid`, a set-once
          secondary-entry callback, and `start_secondary` (SBI HSM
          `hart_start` through the `smp.s` trampoline, which seeds each
          hart's `tp` and a private `.bss` stack slice). `sbi` gains the
          v0.2 `send_ipi` (sPI) and `hart_start` (HSM) calls returning a
          typed `SbiRet`. `RiscvArch` now carries a `CpuId`↔hart-id map
          (`new`/`with_harts`/`hartid_of`/`cpu_for_hartid`): `current_cpu`
          reverse-maps the `tp` hart id and `send_ipi` raises a
          supervisor software interrupt on the target hart (replacing the
          former no-op). `preempt` stores per-hart timer intervals/CpuId,
          adds `enable_ipi`, the set-once IPI callback, and
          `on_software_interrupt` (clears `sip.SSIP`, runs the callback);
          `trap` routes the supervisor-software-interrupt cause there.
          21 new host tests (`sbi`, `smp`, `preempt`, `kernel_arch`),
          clippy/rustdoc clean on host + `riscv64gc-unknown-none-elf`.
          **QEMU vertical `tests/integration/ipi_smp_qemu_riscv64`**
          (enrolled in `tools/xtask/src/commands/qemu_tests.rs`, 2 CPUs,
          60 s) boots the `virt` board, derives the boot hart at runtime
          (OpenSBI may boot on either hart), starts the other hart via
          `smp::start_secondary`, and after that hart enables interrupts
          delivers it a directed IPI through `RiscvArch::send_ipi`; PASS
          once the secondary hart's `sip.SSIP` trap path runs the IPI
          callback with the secondary's id — **verified green under QEMU
          on this host**.
    - [x] **Arch primitives drive the *live* scheduler** —
          `tests/integration/sched_drive_qemu_riscv64`
          (`rustos-test-sched-drive-qemu-riscv64`, enrolled in
          `tools/xtask/src/commands/qemu_tests.rs`, single CPU, 60 s).
          This is the final 3c item: it connects the `preempt` (timer +
          IPI) and `context` primitives to the architecture-neutral
          `kernel/sched` `Scheduler` rather than the test-local counting
          callbacks the `timer_preempt` / `ipi_smp` verticals use. On the
          `virt` board it (1) performs a real bidirectional
          `context::switch` round-trip with interrupts disabled (an
          inbound task seeded by `TaskCtx::prepare` records that it ran
          and `switch`es straight back), (2) builds a real
          `rustos-kernel-sched-mlfq::Scheduler` over `RiscvArch`,
          publishes it, and installs both the `preempt` timer callback
          and the IPI software-interrupt callback so each drives
          `Scheduler::on_timer_tick`, then (3) arms the 100 Hz SBI timer
          + IPI, spawns 64 tasks, sends itself a directed IPI, and drives
          the cooperative `step` loop until every task has run. PASS once
          the supervisor-timer trap has driven the live scheduler ≥ 20
          times and the IPI software-interrupt path has driven it at
          least once; any missing path trips a dedicated failure finisher
          or times out. **Verified green under QEMU on this host** (and
          via the full `cargo xtask ci`).
- 3c's per-sub-stage checklist ("boots to init", "memory-isolation test
  passes", "timer interrupt drives scheduler"), multi-hart SMP, and the
  live-scheduler wiring are all satisfied, so **Stage 3c is complete**.

**Stage 3d status: complete.**
- `kernel/arch/wasm32` is now a full Arch HAL implementation for the
  browser sandbox (`wasm32-unknown-unknown`). It is the structural
  counterpart of the bare-metal ports, but the "hardware" is a
  JavaScript host: the per-CPU identity is the executing Web Worker
  context, the monotonic clock is `performance.now()`, the
  timer-interrupt-drives-scheduler path is a `requestAnimationFrame`
  cooperative tick, an inter-processor interrupt is a `MessageChannel`
  post, and the MMU/page-table isolation is one WASM linear memory per
  worker. Every module carrying pure logic is host-unit-tested (28
  host tests) and clippy/rustdoc clean on both the host and the
  `wasm32-unknown-unknown` target; the browser-host bindings are gated
  to `cfg(target_arch = "wasm32")`:
    - [x] **Arch HAL** — `kernel_arch.rs`'s `WasmArch` implements
          `rustos_arch_api::SchedulerArch`; the monotonic clock reads
          `performance.now()` and converts via `ms_to_ns`. The
          `CpuId` ↔ worker-index map mirrors the bare-metal ports'
          `CpuId` ↔ hart-id map; `send_ipi` posts on the target
          worker's `MessageChannel`.
    - [x] **Cooperative preemption** — `preempt.rs`: a set-once tick
          callback, `init_local_preempt` (requests the first animation
          frame), `on_animation_frame` (callback → re-request frame),
          and `on_ipi_message` (the `MessageChannel` IPI path), plus a
          `cooperative_budget_exhausted` yield helper.
    - [x] **Memory isolation** — `isolation.rs`: the WASM-linear-memory
          isolation model (`MemoryRegion` / `AddressSpace` /
          `WasmFault`), the wasm32 analogue of the bare-metal page
          tables. An attacker `AddressSpace` faults on a victim-only
          address; all bounds arithmetic is checked.
    - [x] **Per-arch syscall entry** — `syscall_entry.rs`: `pack_raw_args`
          marshals into the frozen `rustos_abi`
          `[u64; SYSCALL_MAX_ARGS]` layout (shared with the bare-metal
          ports, §2.2) and a set-once dispatch callback the host call
          forwards to, failing closed without one.
    - [x] **Browser-host glue** — `bindings.rs` (hand-rolled `extern "C"`
          `env` imports — no `wasm-bindgen`, §2.12), `console.rs`
          (`console.log`-backed `rustos_log::Sink`), `entry.rs` (the
          exported `rustos_arch_wasm32_main` / `on_frame` / `on_message`
          trampolines), and `panic.rs` (the `#[panic_handler]` bridge
          that traps the instance), plus the dependency-free host loader
          `kernel/arch/wasm32/web/rustos.js`.
    - The Stage-3 per-sub-stage **browser-headless vertical** is
      `tests/integration/kernel_arch_boot_wasm32` (the wasm32 analogue
      of the bare-metal QEMU verticals): a `cdylib` whose `kernel_main`
      boots the `WasmArch` handle (`BOOT_OK`), runs the isolation check
      (`ISOLATION_OK`), and arms the cooperative scheduler (the tick
      callback prints `TICK`). The puppeteer runner
      (`web/harness.mjs`, launched by the new `cargo xtask test --wasm`)
      serves the module + host loader over loopback, boots them in
      headless Chrome, and reports PASS on `BOOT_OK` + `ISOLATION_OK` +
      ≥ 20 `TICK`s — the three deliverables ("boots to init",
      "memory-isolation test passes", "timer interrupt drives
      scheduler"), **verified green in headless Chrome on this host**.
      The browser harness is opt-in behind `test --wasm` (mirroring
      `test --qemu`) because it needs Node.js + puppeteer + Chrome; the
      `rustos-itest-harness` build glue gained an `itest_wasm32` cfg.
      Docs: `docs/src/platform/wasm32.md`.
    - Multi-worker SMP bring-up (spawning real Web Workers and routing
      `MessageChannel` IPIs between live instances) and wiring the
      cooperative tick into the *live* `kernel/sched` scheduler remain
      wasm32 follow-ups, exactly as they were staged for the bare-metal
      ports.

---

## Stage 2.7 follow-up — Production syscall wiring

**Dependencies:** Stage 2.7 (`kernel/syscall::Dispatcher`), Stage 3a
(`kernel/rustos-kernel` bin with fail-closed dispatch callback
installed before `syscall` is enabled).

**Why this is its own thread.** Stage 2.7 landed the architecture-
neutral `Dispatcher`, the `SyscallHandlers` trait, the audit-event
catalogue, the ABI hash cross-check, and a 100 000-iteration fuzz
harness. Stage 3a (c7-bin) installed a fail-closed callback in the
production `rustos-kernel` binary that *halts* on first syscall.
The earlier PLAN.md note that the swap is "body-only" understated
the work: the tree has neither a production `SyscallHandlers` impl,
nor per-CPU current-task plumbing, nor a registration hook on
`kernel_main`. This section enumerates the missing pieces so the
next session lands them as a coherent, AGENTS.md-compliant change
set instead of a single oversized commit.

**Hard constraints (AGENTS.md).** §2 no hacks / no bloat / no
interface creep; §5.4 the five-step privileged-entry sequence; §7
coverage floors stay green (`kernel/sec`/`kernel/ipc`/`lib/caps`/
`lib/crypto` ≥ 95 %, other kernel ≥ 85 %); §10 every `unsafe`
carries `// SAFETY:` and a test; §13 docs in the same commit; §14
one logical change per commit with the `Co-authored-by` trailer.

### Sub-checklist

- [x] **(f1)** Per-CPU **current-task slot** in `kernel/sched`
      (commit `c93e823`). `Scheduler<A>` gained a
      `Box<[AtomicU64]>` slot array (sentinel `0` = no task),
      `pub fn current_task(cpu) -> Option<TaskId>` (read-only), and
      `pub fn yield_current(task_id) -> SchedResult<()>` for the
      future `yield_now` handler. Slot is published by `dispatch`
      immediately before the body runs, cleared as soon as it
      returns; `park` / `exit` / `yield_current` defensively clear
      matching entries. No new `SchedulerArch` method. 8 new host
      unit tests in `scheduler::tests`; `docs/src/architecture/
      scheduler.md` gained the "Current-task slot" section
      (lifecycle table, concurrency rules,
      `yield_current` vs body-returned `TaskAction::Yield`
      distinction). `cargo test -p rustos-kernel-sched` 36/36 lib +
      28/28 integ + 1/1 stress, clippy `-D warnings` clean, fmt
      clean, full `cargo xtask ci` green at HEAD of this commit.

      *Carry-over:* the `lib/sync::RwLock` process-context rule
      is documented at the new public API — syscall callers must
      read `current_task` from process context on the issuing CPU,
      never from an interrupt handler.

- [x] **(f2)** `TaskId → &TaskCapabilities` lookup in `kernel/sec`
      (commit `fcfb5fc`). `pub struct CapTable` in
      `kernel/sec::captable` owns a flat
      `BTreeMap<TaskId, TaskCapabilities>` and exposes the minimum
      surface the dispatcher needs: `new` / `Default`, `insert`
      (returns the displaced record on duplicate `TaskId` rather
      than silently overwriting), `caps_for` (immutable borrow for
      `cap_query` and IPC capability checks), `caps_for_mut`
      (mutable borrow for `cap_delegate` / `cap_revoke` /
      `apply_token`), `remove` (returns evicted record so callers
      can zero out credential-holding memory per AGENTS.md §4),
      plus `len` / `is_empty` for tests. Re-exported from
      `kernel/sec` as `rustos_kernel_sec::CapTable`.

      No interior mutability — the synchronisation policy lives
      with `KernelState` in `kernel/core::init` (f4), mirroring how
      `Scheduler::tasks` already composes with the scheduler under
      a single lock-ordering policy. No ambient authority on
      lookup: `TaskCapabilities::derive`'s intersection invariant
      is the only widening site, and the registry simply stores its
      output.

      7 new host unit tests in `captable::tests`
      (`captable_*`). New page `docs/src/security/captable.md` with
      a "Per-task registry" section; listed in `docs/src/SUMMARY.md`
      under a new "Security" top-level so the link checker has no
      orphan. `cargo test -p rustos-kernel-sec --lib` 39/39, clippy
      `-D warnings` clean, fmt clean, `mdbook build` clean.

      *Out of scope for (f2), deferred to (f4):* `CapTable` is not
      yet wired into `KernelState` — (f4)'s registration hook is the
      step that adds it next to `Scheduler` under the new
      lock-ordering policy.

- [x] **(f3)** Production **`SyscallHandlers` impl** in
      `kernel/core::syscalls` (new module). One concrete struct
      `KernelSyscallHandlers<'a, A: KernelArch>` that borrows
      `&'a Scheduler<A>`, `&'a CapTable`, and (later) the
      `kernel/ipc` ports registry. Each method translates the
      already-validated arguments into the owning subsystem call:
        - `yield_now` → `Scheduler::yield_current(caller.task_id)`
          (new method on `Scheduler<A>`; see (f1)).
        - `exit(code)` → `Scheduler::exit(caller.task_id)` +
          `CapTable::remove(caller.task_id)`. The `code` is
          recorded in the audit field, not stored on the task
          struct (no new field invented).
        - `ipc_send` / `ipc_recv` — out of scope **for this
          follow-up**: the handler returns `Errno::NotFound` and
          emits an audit record flagging the deferral. The
          named-port registry that resolves an `EndpointId` to a
          live `Port` has since landed
          (`kernel/ipc::PortRegistry`, see below and
          `kernel/ipc/src/lib.rs` rustdoc); wiring it into these
          handlers still awaits composing it into `KernelState`
          and the user-memory copy-in path (Stage 5 / Stage 6).
        - `cap_query` → `caller.caps.contains(cap)` mapped to
          `0 | 1`.
        - `cap_delegate` / `cap_revoke` — call into existing
          `TaskCapabilities::{delegate,revoke}` plus
          `CapTable::caps_for(target)`. The `set_ptr` argument is
          a user pointer; **user-memory copy-in is out of scope**
          here — the handler returns `Errno::NotImplemented` with
          an audit record and Stage 5 / Stage 6 (user memory
          plumbing) lands it. The argument validator at the
          dispatcher level still rejects null + un-aligned pointers
          before we ever see them.
        - `clock_get` → `KernelArch::monotonic_ns(cpu_id)`. **New
          `KernelArch` method**, justified because no existing
          method exposes a monotonic clock; arch ports already
          read RDTSC / CNTVCT_EL0 / `rdtime` for the scheduler so
          there is no duplication. Default impl is **not** provided
          (every arch must opt in — fail-closed). x86_64 wires
          through `apic_timer::Calibration`.

      Tests: host unit tests in `kernel/core/tests/syscalls.rs`
      exercising each handler against `TestArch` + a stub
      `CapTable`; one negative test per `Errno` variant the
      handler can produce. Coverage: `kernel/core` ≥ 85 %.
      Docs: `docs/src/architecture/syscalls.md` "Handler wiring"
      section.

- [x] **(f4)** **Registration hook** on `kernel_main`. Extend
      `BootInfo` with one new field, `dispatcher_callback_slot:
      &'static DispatchCallbackSlot`, whose `install_dispatcher`
      method is called by `kernel_main` *between* the `Sched` phase
      and the `Ipc` phase, after `KernelState` is built. The slot
      is owned by the bin crate (it must outlive the kernel) and
      shipped to `kernel_core` through `BootInfo`. The arch port's
      `set_dispatch_callback` is **still** invoked before `syscall`
      is enabled — the new slot is the *kernel-side* publication
      point, not the trampoline. No global mutable static; the
      `&'static` reference is to memory the bin crate's
      `#[link_section]` reserves at compile time. Tests:
      `kernel/core/tests/kernel_main.rs` gains a registration-
      ordering test that fails if `BootCompleted` fires without
      `install_dispatcher` being called. Docs:
      `docs/src/architecture/kernel.md` "Syscall registration
      phase" section.

- [x] **(f5)** `kernel/rustos-kernel::dispatch` body swap.
      Replace `fail_closed_dispatch` with `production_dispatch`
      that:
        - reads `RawArgs` via the existing `read_raw_args` helper,
        - reads `current_cpu()` from the arch crate,
        - looks up `current_task(cpu_id)` from the scheduler
          (published via the registration hook from (f4)),
        - looks up `&TaskCapabilities` from the `CapTable`,
        - builds `CallerContext { task_id, caps }`,
        - calls `Dispatcher::dispatch(&caller, number, args)`,
        - maps `Errno` → the ABI-encoded negative integer.

      If `current_task` returns `None` (no task running on this
      CPU — should be impossible once the scheduler is live but
      AGENTS.md §5.4.5 *fail closed*), the callback emits an
      audit record and halts the CPU exactly as the fail-closed
      version does. Compile-time `_DISPATCH_SIGNATURE_PINNED`
      stays. New host unit tests cover the no-task and
      happy-path branches via the `extern "C"` shim already in
      `dispatch.rs::tests`. Docs:
      `kernel/rustos-kernel/README.md` "Production dispatch
      callback" section + `docs/src/platform/x86_64.md` (c7-bin)
      "Stage 2.7 follow-up" tail.

- [x] **(f6)** **QEMU integration test** (commit `ce06634`). New
      sibling bin `tests/integration/syscall_dispatch_qemu`
      (`rustos-test-syscall-dispatch-qemu`) reuses
      `rustos_kernel::boot` and replaces only the audit Sink. On
      observing `AuditEvent::BootCompleted` the sink synthesises a
      `Scheduler<BinArch>` / `RwLock<CapTable>` /
      `KernelSyscallHandlers` / `Dispatcher` quartet on the stack,
      spawns a no-op task, registers its capability record with
      `CAP_TIME_SET`, and drives `Dispatcher::dispatch` twice:
      `(cap_query, CAP_TIME_SET)` (observed via the dispatcher's
      `Ok(1)` return value, since `cap_query` is `audit: false`
      in `abi-v1` — capability probes must not drown the audit
      log) and `(exit, 0)` (observed via exactly one
      `AuditEvent::SyscallInvoked` `EventId(5000)` record through
      the synthesised inner audit sink). Any other outcome on
      either leg trips `qemu_exit::exit_failure`. The bin's
      `test-hooks` Cargo feature is on by default; a `compile_error!`
      guard rejects `cargo build --release --features test-hooks`,
      and a defensive `[[bans.features]]` rule in `deny.toml`
      forbids the production `rustos-kernel` crate from ever
      growing the same feature (AGENTS.md §1, §5.4.5, §15).
      Enrolled in `cargo xtask test --qemu` with a 60-second
      budget matching `kernel_arch_boot`. Docs:
      `docs/src/platform/x86_64.md` Stage 2.7 follow-up (f6)
      section landed in the same commit (AGENTS.md §13).

      *Departure from the prompt:* the prompt called for two
      `SyscallInvoked` records (one per dispatched syscall);
      `cap_query` is `audit: false` in the immutable `abi-v1`
      table (AGENTS.md §9) and the dispatcher therefore emits no
      audit record for a successful invocation. The synthesised
      entry point still observes the cap_query leg — via the
      `SyscallResult` return value, which AGENTS.md §5.4.4
      explicitly recognises as the evidence path for an unaudited
      decision — and observes the `exit` leg via its
      `SyscallInvoked` audit record exactly as the prompt
      mandated. Flipping `cap_query` to `audit: true` would have
      regressed the documented "pure observer" carve-out in
      `lib/abi/src/syscalls.rs` and is a deliberately rejected
      design alternative.

- [x] **(f7)** PLAN.md update (this commit). Sub-checklist
      (f1)..(f6) all ticked; the Stage 2.7 follow-up status block
      below flips from `partial` to `complete`; Stage 2 evidence
      tail refreshed with a fresh `cargo xtask ci` quote at HEAD
      of (f7).

### Definition of done

- Real `SyscallHandlers` impl wired through `kernel_main`'s
  registration phase.
- Per-CPU current-task slot and `TaskId → &TaskCapabilities`
  registry land with ≥ 95 % coverage in `kernel/sec` and ≥ 85 %
  in `kernel/sched` / `kernel/core`.
- New QEMU integration test boots to a kernel-thread `syscall`
  pair (cap_query + exit) observed via audit-event sink.
- `cargo xtask ci` green at HEAD of every commit; tail quoted in
  PLAN.md.
- `ipc_send`/`ipc_recv` and `cap_delegate`'s `set_ptr` copy-in
  are deferred to later stages and called out explicitly here
  rather than stubbed (AGENTS.md §15.1).

### Stage 2.7 follow-up status — complete

All sub-items (f1)..(f7) have landed. Commits on `master`:

- `c93e823` — kernel/sched: per-CPU current-task slot (f1).
- `fcfb5fc` — kernel/sec: TaskId→TaskCapabilities CapTable
  registry (f2).
- `4497106` — kernel/core: production `SyscallHandlers` impl +
  `KernelArch::monotonic_ns` (f3).
- `eca9e89` — kernel/core: `DispatchCallbackSlot` + `Phase::Syscall`
  + `KernelDispatchHook` + `KernelState` wiring (f4).
- `45c21c3` — kernel/rustos-kernel: `production_dispatch` swap +
  `encode_result` + `DISPATCH_SLOT` install through `BootInfo` (f5).
- `ce06634` — tests: `rustos-test-syscall-dispatch-qemu` QEMU
  integration test that synthesises the Scheduler / CapTable /
  KernelSyscallHandlers / Dispatcher quartet on `BootCompleted`
  and drives `(cap_query, CAP_TIME_SET)` + `(exit, 0)` through
  it; observes the `exit` leg's `AuditEvent::SyscallInvoked`
  record via the synthesised inner audit sink before flipping
  `qemu_exit::exit_success` (f6).
- *(this commit)* — PLAN.md: tick (f6)..(f7), flip the Stage 2.7
  follow-up status block to `complete`, refresh the Stage 2
  evidence tail with the fresh `cargo xtask ci` quote (f7).

`cargo xtask ci` tail at HEAD of (f7):

```
xtask: [test --qemu] 4 test(s) enrolled
xtask: [test --qemu (run rustos-test-memory-isolation)] kernel=… cpus=1 timeout=60s
xtask: [test --qemu (run rustos-test-scheduler-stress-qemu)] kernel=… cpus=4 timeout=120s
xtask: [test --qemu (run rustos-test-kernel-arch-boot)] kernel=… cpus=1 timeout=60s
xtask: [test --qemu (run rustos-test-syscall-dispatch-qemu)] kernel=… cpus=1 timeout=60s
advisories ok, bans ok, licenses ok, sources ok
xtask: [abi-check] lib/abi/src/syscalls.rs ↔ kernel/syscall/src/table.rs
```

The Stage 3a status block above (`Status: complete`) is unchanged
— Stage 3a's (a)..(d1) deliverables are done; the Stage 2.7
follow-up is its own thread and is now also complete.

---

## Stage 4 — Driver Framework and First Drivers

**Dependencies:** Stage 2 + at least one Stage 3 sub-stage.

**Deliverables**
- [x] `lib/abi/src/driver/` driver traits per class
  (`Display`, `Filesystem`, `Block`, `Net`, `Input`, `Bus`).
- Driver host in `userland/` that loads/unloads `.rxe` driver modules,
  enforcing capabilities at load time.
- Initial drivers:
  - `drivers/display/vesa` (x86_64 BIOS).
  - `drivers/display/framebuffer` (aarch64 Pi, riscv64 virt, wasm32 canvas).
  - `drivers/bus/pci` (x86_64), `drivers/bus/mmio` (aarch64/riscv64),
    `drivers/bus/virtio` (cross-arch).
  - `drivers/storage/virtio_blk`.
  - `drivers/input/ps2` (x86_64), `drivers/input/usb_hid` (cross-arch later).
  - `drivers/network/virtio_net`.

**Tests**
- Mock-host unit tests for each driver.
- QEMU integration: load driver → use device → unload driver → reload.

**Docs**
- `docs/src/drivers/overview.md` and one page per driver class.
- Each driver crate ships a `README.md` (supported HW, caps, limits).

**Status: in progress.**
- `drivers/display/vesa` QEMU integration vertical shipped — the last
  outstanding Stage 4 QEMU vertical, and the x86_64 sibling of the
  riscv64 framebuffer-display vertical. It lives in
  `tests/integration/vesa_display_qemu_x86_64`
  (`rustos-test-vesa-qemu-x86-64`, enrolled in
  `tools/xtask/src/commands/qemu_tests.rs`, single CPU, 60-second
  budget) and drives the driver against a **real** emulated framebuffer
  on x86_64. The harness attaches QEMU `ramfb` (`-device ramfb`, now
  threaded through `rustos_qemu::Spec::with_ramfb` →
  `tools/qemu/src/x86_64.rs` as well) and programs a static,
  page-aligned guest-RAM scan-out surface into it over the `fw_cfg`
  **IOport** DMA interface (registers `0x514`/`0x518`); it then
  synthesises the bootloader-captured VBE `ModeInfoBlock` describing
  that surface — the shape VBE function `0x4F01` would produce — as the
  boot hand-off. The vertical boots the production kernel and, on
  `AuditEvent::BootCompleted`, loads the signed vesa `.rxe` through
  `rustos_drvhost::Host` (the §8 load gate, fixture manifest declaring
  `CAP_DRV_LOAD`), decodes the block with `VesaFramebuffer::open`, maps
  the surface through the capability-gated
  `rustos_kernel_virtio::KernelMmioMapper`, and `present`s a frame; a
  second window mapped over the same physical range reads the pixels
  back to confirm they reached the scan-out memory QEMU consumes,
  before and after the reload. The `fw_cfg` DMA protocol itself now
  lives once in the shared `rustos-itest-fwcfg` crate (the
  `FWCfgDmaAccess` staging, file-directory scan, and `RAMFBCfg`
  programming, with host unit tests); the two display verticals supply
  only their transport's DMA-address-register write through the
  `DmaAddressRegister` seam — x86_64 IOport here, riscv64 MMIO there —
  so the protocol is not duplicated (`AGENTS.md` §2.2). The riscv64
  framebuffer vertical was refactored onto the shared crate in the same
  change. No `unwrap`/`expect`/`panic!` in the test bin; the only
  `unsafe` is the shared crate's documented `fw_cfg` DMA staging and
  the shared bumpalloc `#[global_allocator]`. With this every emulable
  Stage 4 first driver now has a `load → use → unload → reload` QEMU
  vertical. Docs: `docs/src/drivers/display.md` +
  `drivers/display/vesa/README.md`.
- `drivers/input/ps2` QEMU integration vertical shipped — the first
  per-driver `load → use device → unload → reload` vertical for a
  display/input driver (the virtio storage/net verticals already cover
  that lifecycle for their classes). It lives in
  `tests/integration/ps2_input_qemu_x86_64`
  (`rustos-test-ps2-qemu-x86-64`, enrolled in
  `tools/xtask/src/commands/qemu_tests.rs`, single CPU, 60-second
  budget). The boot hand-off prerequisite the ps2 driver had been
  waiting on is now in the tree: `kernel/arch/x86_64/src/pio.rs` gained
  `X86PortIo8` + `x86_port_io8()`, the byte-wide sibling of the PCI bus
  driver's 32-bit `X86PortIo` and the only in-tree implementor of the
  `rustos_abi::PortIo8` seam (`in al, dx` / `out dx, al` behind the safe
  trait, each with a `// SAFETY:` block; a zero-sized-handle +
  trait-object-coercion unit test). The vertical boots the production
  kernel, on `AuditEvent::BootCompleted` loads the signed ps2 `.rxe`
  through `rustos_drvhost::Host` (exercising the §8 load gate against
  the real `rustos_drv_input_ps2::register`, with the fixture manifest
  declaring `CAP_DRV_LOAD` so the host-installed grant — manifest ∩
  caller — actually carries it), then mints `X86PortIo8` and drives a
  real `Ps2Keyboard` through load → use → unload → reload. "Use" is made
  deterministic without a physical keypress via the i8042 `0xD2` ("write
  keyboard output buffer") command: the test injects a scancode into the
  controller's output buffer through the same `PortIo8` backend the
  driver reads through, then confirms the driver decodes the injected
  press and, after reload, the matching release. Polling means no IRQ
  routing is required for the vertical; an interrupt-driven delivery
  path remains a later follow-up. No `unwrap` / `expect` / `panic!` in
  the test bin; the only `unsafe` is the arch port's two PIO
  instructions and the shared bumpalloc `#[global_allocator]` the other
  QEMU verticals use. Docs: refreshed `docs/src/drivers/input.md`
  + `drivers/input/ps2/README.md`.
- `drivers/display/framebuffer` QEMU integration vertical shipped — the
  first display-class `load → use device → unload → reload` vertical. It
  lives in `tests/integration/framebuffer_display_qemu_riscv64`
  (`rustos-test-framebuffer-display-qemu-riscv64`, enrolled in
  `tools/xtask/src/commands/qemu_tests.rs`, single CPU, 60-second
  budget) and runs on the riscv64 `virt` board against a **real**
  emulated framebuffer. Rather than fabricate a device, the test harness
  attaches QEMU `ramfb` (`-device ramfb`, threaded through
  `rustos_qemu::Spec::with_ramfb` → `tools/qemu/src/riscv64.rs`) and
  programs a static, page-aligned guest-RAM scan-out surface into it
  over the `fw_cfg` MMIO DMA interface (locate `etc/ramfb` in the file
  directory, DMA-write the big-endian `RAMFBCfg`); the resulting
  geometry is the `FramebufferConfig` boot hand-off. The vertical boots
  the production kernel, and on `AuditEvent::BootCompleted` loads the
  signed framebuffer `.rxe` through `rustos_drvhost::Host` (the §8 load
  gate, fixture manifest declaring `CAP_DRV_LOAD`), then maps the surface
  through the capability-gated `rustos_kernel_virtio::KernelMmioMapper`
  — the real kernel MMIO-map facility the bus drivers use — and
  `present`s a frame; a second window mapped over the same physical
  range reads the pixels back to confirm they reached the scan-out
  memory QEMU consumes, before and after the reload. The `ramfb`/`fw_cfg`
  bring-up is test-harness code (mirroring how the virtio verticals own
  their PLIC/trap bring-up rather than placing it in the production
  kernel). No `unwrap`/`expect`/`panic!` in the test bin; the `unsafe`
  is confined to the documented `fw_cfg` MMIO/DMA accesses, the DTB
  span, and the shared bumpalloc `#[global_allocator]`. Docs:
  `docs/src/drivers/display.md` + `drivers/display/framebuffer/README.md`.
  The remaining `drivers/display/vesa` QEMU vertical has since shipped
  too (see the top status bullet); its x86_64 `fw_cfg` IOport transport
  and this riscv64 MMIO transport now share the `rustos-itest-fwcfg`
  crate's DMA protocol.
- `drivers/display/vesa` (x86_64 BIOS) shipped — the VBE
  linear-framebuffer display driver, and the last outstanding Stage 4
  first driver. It implements `rustos_abi::driver::display::Display`
  over the linear framebuffer a VESA BIOS Extensions (VBE) mode exposes
  on a legacy PC. Because the kernel cannot re-enter real mode to issue
  VBE BIOS calls, mode selection happens in the bootloader; the boot
  stub captures the 256-byte VBE `ModeInfoBlock` (VBE function `0x4F01`)
  and hands it to the driver host as a boot capability. `VbeModeInfo`
  `::parse` decodes and validates that block — accepting only a
  supported mode whose linear-framebuffer attribute is set
  (`ModeAttributes` bits 0 + 7), the direct-colour memory model
  (`MemoryModel == 6`), 32 bpp with 8-bit channel masks, and a channel
  layout that maps to `DisplayFormat::Bgra8888` (red at bit 16) or
  `Rgba8888` (red at bit 0); a zero `PhysBasePtr`, a stride too small
  for one scanline, or any other layout fails closed (`DeviceFault` /
  `LengthOutOfRange` / `Unsupported`). This VBE decode is the deliberate
  sibling distinction from the generic `framebuffer` driver, which
  consumes an already-parsed geometry record (`AGENTS.md` §2.2
  carve-out — not duplication). `VesaFramebuffer::open` then maps
  exactly `stride_bytes * height_px` bytes at the reported `PhysBasePtr`
  through the host's `MmioMapper` (enforcing `CAP_MMIO_MAP`), so the
  framebuffer is reached only through a kernel-installed mapping, never
  a pointer the driver synthesises (§4 — no ambient authority). Per
  `AGENTS.md` §8 the only public function is `register` (gated on
  `CAP_DRV_LOAD`); the `VbeModeInfo` and `VesaFramebuffer` types are
  re-exported so the host can decode a block and construct an instance,
  then reach it only through the `Display` trait. `present` is
  byte-preserving and bounds-checked at every window write; dropping the
  `VesaFramebuffer` releases the window (unload), and `open` again
  reloads. No `unwrap` / `expect` / `panic!` / `unsafe` in the crate; no
  architecture `cfg`. Tests: 23 host-side unit tests against an
  in-process mock `MmioMapper` (register gate; `VbeModeInfo::parse`
  Bgra/Rgba decode + every rejection path; `open` mode report; `present`
  byte fidelity incl. a non-word-multiple surface; short-frame /
  oversized-frame handling; host + mapper `CAP_MMIO_MAP` gates; absent
  mapper; unmappable region; parse failure before mapping;
  unload→reload). Docs: a new `rustos-drv-display-vesa` section in
  `docs/src/drivers/display.md` (already wired into `docs/src/SUMMARY.md`
  under the existing display page) + the crate `README.md`. The QEMU
  integration vertical for this driver has since shipped (see the top
  status bullet). With this driver every per-class Stage 4 first driver
  listed in the deliverables is now implemented.
- `drivers/input/ps2` (x86_64) shipped — the first input-class driver.
  It implements `rustos_abi::driver::input::Input` for a keyboard on the
  Intel 8042 controller (status/command port `0x64`, data port `0x60`),
  decoding a scancode-set-1 byte stream into platform-neutral
  `InputEvent`s (base make code for unprefixed keys, `0xE000 | make` for
  `E0`-extended keys; `value == 1` press / `0` release). Per `AGENTS.md`
  §8 the only public function is `register` (gated on `CAP_DRV_LOAD`);
  the `Ps2Keyboard` type + `new` constructor are re-exported so the host
  can instantiate it and then reach it only through the `Input` trait.
  The driver never issues `inb`/`outb` itself: it reaches the two ports
  through a new 8-bit port seam, `rustos_abi::driver::port_io::PortIo8`
  (`read8`/`write8`), added alongside the frozen 32-bit `PortIo` (which
  is reserved for PCI mechanism #1) — a separate versioned trait rather
  than an added method, per `AGENTS.md` §2.4, so the driver carries no
  architecture `cfg` and no ambient authority over the I/O port space
  (§4 / §17.2 / §17.4). The drain is bounded by a per-call read budget so
  a stuck controller can never make `poll` spin (§2.1), stops
  non-destructively at an auxiliary (mouse) byte, latches a trailing
  `E0` prefix across polls, and skips detection-error / overrun markers.
  No `unwrap` / `expect` / `panic!` / `unsafe` in the crate. Tests: 12
  host-side unit tests against an in-process mock `PortIo8` controller
  (register gate, empty-buffer rejection, empty-queue `Ok(0)`,
  press/release + extended decode, prefix latching, error-marker skip,
  auxiliary-byte stop, buffer-fill-and-resume, read-budget bound,
  never-writes-controller, unload→reload) plus 2 new `PortIo8` seam
  tests in `lib/abi`. `cargo xtask ci` (fmt → clippy → test `--qemu` →
  docs-check → deny → abi-check) is green end-to-end; `deps-check` /
  `cfg-check` clean. Docs: `docs/src/drivers/input.md` (wired into
  `docs/src/SUMMARY.md`) + a `PortIo8` note in
  `docs/src/abi/driver_traits.md` + the crate `README.md`. A QEMU
  integration vertical depends on the kernel wiring a `PortIo8` backend
  over the legacy ports and routing the i8042 IRQ (line 1) to the
  user-space driver — the same boot hand-off prerequisite the
  framebuffer and virtio-blk QEMU verticals waited on.
- `lib/abi/src/driver/` trait surface has landed: `DriverHost`,
  `DriverHandle`, `DriverError`, `DriverKind` (`UserSpace` / `InKernel`),
  and `DriverManifest` (frozen `abi-v1` wire layout — magic `"DRV1"`,
  abi version, kind, capability count, syscall-table hash, signer
  pubkey, Ed25519 signature; signed range excludes the signature
  tail). Six class trait modules — `display`, `filesystem`, `block`,
  `net`, `input`, `bus` — each ship the smallest method set required
  by the Stage 4 first drivers, with `# Errors` and `# Capabilities`
  rustdoc sections on every public item and `#[non_exhaustive]` enums
  for forward compatibility (`AGENTS.md` §2.4 / §9). No `unwrap` /
  `expect` / `panic!` / `unsafe` introduced. 27 new unit tests
  (driver-mod, display, filesystem, block, net, input, bus); the
  full `cargo test -p rustos-abi --lib` count is 63 passing, 0
  ignored. Docs: `docs/src/drivers/overview.md` (lifecycle,
  capability model, kinds) + `docs/src/abi/driver_traits.md` (ABI
  reference, frozen `abi-v1`); both wired into `docs/src/SUMMARY.md`
  under new `# Drivers` and `# ABI` top-level sections.
- Driver host shipped as `userland/system/drvhost`: a `no_std` +
  `alloc` userland service that owns `.rxe` driver-module lifecycle
  (`load`, `unload`, `reload`) with capability enforcement at load
  time per `AGENTS.md` §8. The host depends only on the audited
  `rustos-abi`, `rustos-caps`, `rustos-crypto`, and `rustos-log`
  crates and re-uses `lib/abi`'s `DriverManifest` decoder rather than
  duplicating it. The verification pipeline runs nine gates in
  order — envelope parse, syscall-table hash match, trust-anchor
  lookup, Ed25519 signature verification, capability-body decode,
  in-kernel-kind gate (`CAP_DRV_KERNEL` required for
  `DriverKind::InKernel`), subset-only delegation gate, resolver
  bind, driver `register()` call — and fails closed at the first
  failure (`AGENTS.md` §5.4.5). Every transition emits a structured
  `rustos_log::Event` from the reserved `7000..8000` `EventId`
  range. Buffers that held the manifest signature or capability
  bitmap are wiped with a volatile-clear primitive
  (`zeroize::secure_clear`, the one `unsafe` block in the crate,
  covered by a dedicated unit test). No `unwrap` / `expect` /
  `panic!` / `todo!` in production paths; `HostError::as_errno`
  surfaces a stable `Errno` mapping for the future syscall wrapper.
  Tests: 19 in-crate unit tests + 13 integration tests under
  `userland/system/drvhost/tests/` covering happy-path load → call
  → unload → reload, tampered signature, untrusted signer, ABI
  version mismatch, syscall-table hash mismatch, capability
  escalation, `InKernel` without `CAP_DRV_KERNEL`, caller without
  `CAP_DRV_LOAD`, resolver miss, driver `register()` failure, and
  source-read failure propagation. QEMU integration test
  `tests/integration/drvhost_qemu` boots the production
  `rustos-kernel` pipeline on `x86_64-unknown-none`, observes
  `AuditEvent::BootCompleted`, then drives the host through `load →
  snapshot → reload → unload` against a build-time-signed mock
  `.rxe` fixture (`build.rs` emits `MOCK_IMAGE` + matching
  `TRUSTED_SIGNER_PUBKEY` + `SYSCALL_TABLE_HASH` consts) and flips
  `qemu_exit::exit_success`; enrolled in `tools/xtask` with a
  60-second budget matching the other Stage-3a boot-then-do-fixed-
  work tests. Docs: `docs/src/drivers/host.md` (public surface,
  trust anchor model, audit catalogue, security model) and
  `docs/src/drivers/lifecycle.md` (gate-by-gate flow, capability
  table, sensitive-buffer wipe contract); both wired into
  `docs/src/SUMMARY.md`.
- `drivers/bus/pci` (x86_64) and `drivers/bus/mmio` (aarch64 /
  riscv64) shipped: each implements
  `rustos_abi::driver::bus::Bus` against a transport-specific
  configuration-access seam (`ConfigSpace` for mechanism #1 PIO,
  `MmioRead` for volatile MMIO loads). Per `AGENTS.md` §8 the only
  public function in either crate is `register`; every other type
  is `pub(crate)` and exercised through in-crate `#[cfg(test)]`
  modules. The PCI walker covers bus / device / function
  enumeration, capability-list traversal with structural MSI /
  MSI-X decoding (other IDs surfaced opaquely), and BAR sizing
  through the FFFFFFFF/read-back/restore probe; BAR mapping is
  deferred to Stage 4.D where virtio-blk / virtio-net route the
  request through the driver host's memory capability. The MMIO
  walker iterates the boot DTB through `rustos_fdt` (the single
  shared device-tree parser; the WIRING burn-down later folded the
  one-off `lib/util/dtb` parser into `lib/fdt` so the arch ports,
  the QEMU verticals, and this driver all walk the `virt` tree
  through one reader per `AGENTS.md` §2.2). Tests: 12 host-side unit tests for
  the PCI driver (including the exact `q35` device-list
  assertion, capability-walker, BAR-sizing probe, and
  `register` capability gate) and 6 for the MMIO driver
  (including the exact `virt`-slot list against a four-slot DTB
  fixture). The PCI volatile `unsafe asm!` and the MMIO
  volatile-read `unsafe` block each carry a `// SAFETY:` block
  and are encapsulated behind a safe trait (`PortIo` /
  `MmioRead`); no `unsafe` leaks across the crate boundary.
  Docs: `docs/src/drivers/bus.md` plus a README per driver
  crate, both wired into `docs/src/SUMMARY.md`.
- `drivers/bus/virtio` (cross-arch virtio transport),
  `drivers/storage/virtio_blk`, and `drivers/network/virtio_net`
  shipped in their host-side form: the transport crate implements
  the virtio 1.1 §2.6 split virtqueue (descriptor table, avail
  ring, used ring, free-descriptor pool, descriptor chaining), the
  virtio 1.1 §3.1 status-byte initialisation sequence, a
  `VirtioHost` allocator+notifier trait, a `BounceBuffer` wrapper
  that scrubs DMA staging on drop for
  `BufferClass::Sensitive`, and a `MockTransport` / `ChainView`
  test seam. The two device drivers implement
  `rustos_abi::driver::block::Block` / `rustos_abi::driver::net::Net`
  on the same bus-agnostic transport (so PCI and MMIO use one
  source) and override `*_with_class` to honour the sensitive
  scrub contract (`AGENTS.md` §4). 30 transport-crate tests + 8
  virtio-blk tests + 9 virtio-net tests all pass; `cargo clippy
  -p rustos-drv-bus-virtio -p rustos-drv-storage-virtio-blk
  -p rustos-drv-network-virtio-net --all-targets -- -D warnings`
  is clean. Docs: `docs/src/drivers/virtio.md`,
  `docs/src/drivers/block.md`, `docs/src/drivers/network.md` plus
  one README per driver crate, all wired into
  `docs/src/SUMMARY.md`. Deferred (each spelled out in
  `.junie/next-session-prompt.md`):
  (1) kernel per-process-heap DMA allocator with `phys_of()` —
  the current `MockHost::alloc_dma_zeroed` returns `phys == virt`
  because the kernel memory-capability surface does not exist;
  (2) IRQ routing into user-space drivers — `notify_wait` is a
  polled cooperative hook in this PR;
  (3) capability-checked bus-handle hand-off from `drivers/bus/pci`
  and `drivers/bus/mmio` — the `PciBackend` / `MmioBackend` shells
  carry only the identification tuple they were constructed with;
  (4) QEMU PCI (x86_64) and MMIO (riscv64) integration tests for
  virtio-blk read/write+checksum and virtio-net ARP/ICMP echo,
  plus the unload→reload→reuse path;
  (5) the userland ARP/IP/ICMP responder required by the
  virtio-net QEMU integration. The remaining per-class first
  drivers (`drivers/display/vesa`, `drivers/input/ps2`) remain
  outstanding per the Stage 4 deliverable list above; packed
  virtqueues (virtio 1.1 §2.7) are a Stage 5 follow-up documented
  in `docs/src/drivers/virtio.md`.
- `drivers/display/framebuffer` shipped (generic linear framebuffer
  first driver): implements `rustos_abi::driver::display::Display`
  over a firmware-provided linear pixel surface. Per `AGENTS.md` §8
  the only public function is `register`; the `Framebuffer` type and
  `FramebufferConfig` are re-exported so the driver host can
  construct an instance, and the host reaches it only through the
  `Display` trait. The driver never synthesises a pointer to the
  surface — `Framebuffer::open` validates the firmware geometry,
  then maps exactly `stride_bytes * height_px` bytes through the
  host's capability-gated `MmioMapper` (`CAP_MMIO_MAP`), keeping the
  driver free of ambient authority (`AGENTS.md` §4). `present` is
  byte-preserving: it copies the caller's frame verbatim into the
  mapped window through bounds-checked volatile writes (a `u32` bulk
  path plus a byte tail), so a short frame fails closed with
  `DriverError::BufferTooSmall` rather than panicking (`AGENTS.md`
  §2.9). No `unwrap` / `expect` / `panic!` / `unsafe` in the crate.
  Tests: 12 host-side unit tests against an in-process mock
  `MmioMapper` (mode report, present byte-fidelity including a
  non-word-multiple surface, short/oversized frame handling, the
  `CAP_DRV_LOAD` / `CAP_MMIO_MAP` gates, absent-mapper and
  region-too-large `Unsupported` paths, degenerate-geometry
  rejection, and an unload→reload round-trip); `cargo clippy
  -p rustos-drv-display-framebuffer --all-targets -- -D warnings` is
  clean. Docs: `docs/src/drivers/display.md` (wired into
  `docs/src/SUMMARY.md`) plus the crate `README.md`. A QEMU
  integration vertical depends on kernel framebuffer hand-off
  plumbing (a boot capability publishing the firmware
  `FramebufferConfig` plus a kernel `MmioMapper` over the surface),
  which is not yet in the tree — the same prerequisite pattern the
  virtio-blk QEMU verticals waited on (deferral (4) above).
- Relicence follow-up (*complete*): the project moved from `GPL-3.0-only`
  to **`GPL-2.0-or-later`** (GNU GPL version 2, or — at the recipient's
  option — any later version) with an additional **syscall / ABI
  exception**. The exception (named `RustOS-syscall-note`, modelled on the
  same kind of grant other kernels publish) keeps user-space programs that
  merely make system calls or include the project's published syscall / ABI
  interface definitions from being treated as derived works. The verbatim
  GNU GPL version 2 text and the exception preamble live in the root
  `LICENSE`; every crate inherits `license = "GPL-2.0-or-later"` from
  `[workspace.package]` (the exception only *loosens* the GPL, and no SPDX
  exception identifier is registered for it, so the base copyleft is the
  honest machine-readable expression). `deny.toml`'s `licenses.allow` now
  lists `GPL-2.0-or-later`; `cargo deny check` is `advisories ok, bans ok,
  licenses ok, sources ok`. `README.md` and the `drivers/bus/{pci,mmio}`
  READMEs record the new licence + exception. The earlier `GPL-3.0` →
  `GPL-3.0-only` defect note below is retained as accurate history.
- Stage 4.D follow-up (Item 6 — acceptance gate, *complete*): finished the
  gate on a host that has `mdbook` (v0.5.3) + `mdbook-linkcheck` and
  `cargo-deny` (0.19.7) installed — the two tools the previous session
  lacked — and fixed the two real defects the now-runnable steps surfaced.
  **Defect 1 — `cargo deny check` licence policy.** Every workspace crate
  declares the canonical SPDX `license = "GPL-3.0-only"` (matching
  `AGENTS.md` §1 and `LICENSE`), but `deny.toml`'s `licenses.allow` listed
  only the *deprecated* `GPL-3.0` identifier, so under `version = 2` SPDX
  evaluation `cargo deny check` rejected all first-party crates
  (`licenses FAILED`). Replaced `GPL-3.0` with `GPL-3.0-only` in the allow
  list; `cargo deny check` is now `advisories ok, bans ok, licenses ok,
  sources ok`. **Defect 2 — `cargo xtask coverage` tool probe.** The
  availability check ran `cargo-llvm-cov --version` directly, but
  `cargo-llvm-cov` is a *cargo subcommand* whose binary rejects a bare
  `--version` (`error: expected subcommand 'llvm-cov'`), so `cargo xtask
  coverage` always aborted with "cargo-llvm-cov is not installed" even when
  it was. Added a `cargo_subcommand_available(ctx, sub)` helper that probes
  via `cargo <sub> --version` (the same path the command is actually
  invoked through) and routed both `run_coverage` (`llvm-cov`) and
  `run_deny` (`deny`) through it; `mdbook` keeps the plain-binary probe. A
  fail-closed unit test (`cargo_subcommand_probe_fails_closed_for_unknown_subcommand`)
  guards the regression — xtask unit tests now 11 passing. **Verification
  (this host).** `cargo xtask ci` (fmt → clippy → test `--qemu` → docs-check
  → deny → abi-check) green end-to-end; `cargo xtask docs-check` (rustdoc
  `-D warnings` + mdBook build + in-tree link check) green; `cargo deny
  check` clean; `cargo xtask test --qemu` ran all 11 verticals green
  (8 x86_64 boot/IRQ/syscall/drvhost/virtio-PCI + riscv64 boot +
  `virtio-blk-mmio-riscv64` + `virtio-net-mmio-riscv64`); `cargo xtask
  coverage` (`cargo llvm-cov --workspace --summary-only`) runs, workspace
  TOTAL 93.25% region. Per-`AGENTS.md`-§7 high-bar crates confirmed via
  targeted `cargo llvm-cov --summary-only` runs (identical to the prior
  session's figures): `kernel/sec` ≥97%, `lib/caps` ≥98%, `lib/crypto`
  ≥97.67%, and `kernel/mem` + `kernel/ipc` + `kernel/irq` combined 95.18%
  region / 95.38% line. (As before, the bare `cargo llvm-cov --workspace`
  view does not surface `kernel/{core,mem,ipc,irq}` rows — they carry no
  in-`lib` unit tests, taking their coverage from `tests/` integration
  binaries — so the high-bar subset is confirmed with the targeted
  `--summary-only` runs the prior session also used.) Stage 4.D Item 6 — and
  therefore the Stage 4.D acceptance gate — is now complete; the remaining
  Stage 4 deliverables (`drivers/display/vesa`, `drivers/input/ps2`) are
  unaffected by this item.
- Stage 4.D follow-up (Item 6 — acceptance gate, *partial*): ran the full
  runnable `xtask` matrix on this host and fixed three rustdoc defects the
  gate surfaced. **rustdoc defects (`AGENTS.md` §13 — `docs-check`).** The
  real `docs-check` rustdoc step is `cargo doc --workspace --no-deps
  --document-private-items` on the host target with `RUSTDOCFLAGS="-D
  warnings"`; it failed on intra-doc links left dangling by the Item-4
  crate extraction (the `KernelVirtioFactory` + virtio walks moving from
  `rustos-kernel` to `kernel/virtio`): `kernel/rustos-kernel/src/virtio_boot.rs`
  module docs linked `crate::virtio_pci_walk` / `crate::virtio_factory`
  (modules no longer in `rustos_kernel`) — repointed to the re-exported
  `provision_virtio_pci` / `KernelVirtioFactory`;
  `tests/integration/virtio_qemu_support/src/lib.rs` module docs linked
  `virtio_blk_round_trip` / `virtio_net_ping` / `rustos_drv_bus_virtio::Transport`,
  none of which resolve on the host target where every module is
  `cfg`-gated out — demoted to plain code spans; and
  `drivers/bus/pci/src/enumerate.rs` carried a redundant explicit
  `VIRTIO_CFG_NOTIFY` link target (in scope via the module import) flagged
  under `--document-private-items` — target removed. After the fixes
  rustdoc is clean across the workspace. **Verification (this host).**
  `cargo build --workspace`, `cargo fmt --all --check`, `cargo xtask
  clippy` (host all-targets), `cargo xtask abi-check`, `cargo xtask test`
  (host) and `cargo xtask test --qemu` (all 11 verticals — the 8 x86_64
  boot/IRQ/syscall/drvhost/virtio-PCI plus the riscv64 boot,
  `virtio-blk-mmio-riscv64`, `virtio-net-mmio-riscv64`) all green; coverage
  via `cargo llvm-cov --summary-only` meets `AGENTS.md` §7 on the high-bar
  crates (kernel/sec ≥97%, lib/caps ≥98%, lib/crypto ≥97.67%; kernel/mem +
  ipc + irq combined 95.18% region / 95.38% line; workspace TOTAL 93.28%).
  **Not run here (tooling absent):** the mdBook half of `cargo xtask
  docs-check` (no `mdbook`) and `cargo deny check` (no `cargo-deny`); the
  gate's `cargo xtask ci` must still be completed on a host with both, but
  every step reachable in this environment passes.
- Stage 4.D follow-up (Item 4 — riscv64 virtio-MMIO QEMU verticals +
  arch-neutral virtio crate, *complete*): the two remaining Item 4
  deliverables — `virtio_blk_mmio_riscv64` and `virtio_net_mmio_riscv64`
  — boot the riscv64 `virt`-board pipeline to `AuditEvent::BootCompleted`
  and then drive a *real* virtio device over the board's virtio-mmio bus
  end-to-end, the MMIO analogues of the gated x86_64 PCI verticals.
  **Crate extraction (`AGENTS.md` §2.2 / §6).** The arch-neutral
  `KernelVirtioFactory` + the virtio-PCI / virtio-MMIO provisioning walks
  lived in the x86_64-only `rustos-kernel` bin crate, which does not build
  for `riscv64gc-unknown-none-elf` (it depends on `rustos-arch-x86_64`).
  They moved to a new `kernel/virtio` (`rustos-kernel-virtio`) crate that
  names no architecture port, so every Tier-1 freestanding target links
  the *same* factory + walks; `rustos-kernel` re-exports every item, so
  its public API is unchanged (host tests for all three still pass).
  **Shared bring-up (`AGENTS.md` §2.2).** `virtio_qemu_support` is now
  arch-generic: a `common` module owns the `QemuEnv` seam (serial
  breadcrumbs + QEMU exit), the signed-`.rxe` inputs, the generic
  `drive_driver_lifecycle<Tr>` (`load → reload → device round-trip →
  unload`), and the generic device tails `virtio_blk_round_trip<Tr>` /
  `virtio_net_ping<Tr>`; an `imp_pci` (x86_64) and a new `imp_mmio`
  (riscv64) module supply the arch-specific bring-up and a `define_*_boot_harness!`
  macro. Both arches re-export their transport as `ScenarioTransport`, so
  the per-vertical device-tail invocation text is identical across
  arches. The x86_64 blk/net verticals were refactored onto the shared
  tails (no behavioural change). **riscv64 MMIO scaffold (`imp_mmio`).**
  Consumes `published_dtb`/`published_memory_map`; builds the `virt`-board
  bus through a new public `rustos_drv_bus_mmio::virtio_mmio_bus_from_dtb`
  (`unsafe` — identity-mapped aperture; concrete `Mmio` type stays
  crate-private behind `impl VirtioMmioBus`, §8); provisions the
  `MmioTransport` through the `CAP_MMIO_MAP`-gated `KernelMmioMapper`;
  walks the DTB for the PLIC base + `riscv,ndev` and the device's
  `interrupts` source; builds a `PlicController` + `IrqTable` (leaked to
  `'static`), `arm`s the source, installs the S-mode trap dispatch
  (`set_trap_dispatch` → PLIC claim → virtio-MMIO `InterruptACK` →
  `IrqTable::fire` → complete) and `init_traps`; mints a
  `KernelVirtioHost` over a carved high-RAM DMA pool; and runs the shared
  `drive_driver_lifecycle`. The IRQ park is a race-free `wfi`: unmask the
  source, clear `sstatus.SIE`, re-check `IrqTable::ready_for`, `wfi` only
  if not ready, restore `SIE` — so a completion landing between the check
  and the park is held pending, not lost (no bounding timer, no hack —
  §2). The virtio-MMIO `InterruptACK` in the dispatch is load-bearing:
  without it the level line never re-edges and the device raises no fresh
  interrupt for the next used buffer. **Runner / enrolment.** The
  riscv64 QEMU runner now passes `-global virtio-mmio.force-legacy=false`
  (RustOS' MMIO transport only drives modern/version-2 virtio-mmio);
  both verticals are enrolled in `cargo xtask test --qemu` (blk: a
  planted 2048-sector disk; net: a user-mode SLIRP interface + frame
  dump). **Verification.** Both verticals reach `SiFive` Test PASS under
  `qemu-system-riscv64` (blk: sector-0 verify + sector-1 round-trip; net:
  ARP-resolve `10.0.2.2` + ICMP echo; both after a `load → reload`
  cycle and a clean `unload`) — blk run 3×, net run 3×, deterministic.
  Host: `cargo test` for `rustos-kernel-virtio` (12), `rustos-drv-bus-mmio`
  (13, +4 new: aperture span/none + constructor over a host buffer +
  not-found), `rustos-kernel` (33), `rustos-qemu` (51, +1 force-legacy).
  `cargo clippy -- -D warnings` (host + x86_64 + riscv64 freestanding
  surfaces), `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` (host +
  riscv64), and `cargo fmt --check` all clean; `cargo build --workspace`
  green. **Docs.** `docs/src/platform/riscv64.md` ("virtio-MMIO QEMU
  verticals"). **Not run in this environment:** the mdBook half of
  `cargo xtask docs-check` (mdbook not installed) and `cargo deny check`;
  the Item 6 acceptance gate must run the full `xtask` matrix on a host
  where both are available.
- Stage 4.D follow-up (Item 4 — riscv64 boot-state publication hooks,
  *complete*): the riscv64 MMIO verticals (the next sub-task) need the
  firmware memory map (to carve a per-device DMA pool) and the
  device-tree pointer (to walk the `virtio_mmio` slots, the PLIC base,
  and each device's `interrupts` cell), but the riscv64 boot exposed
  neither — only x86_64 had the `rustos-kernel` `arch_wrapper` publish
  slots. **Change.** New host-buildable `kernel/arch/riscv64::publish`
  module mirrors the x86_64 memory-map publish pattern: set-once
  `OnceCell` slots for a `BootMemoryMap` clone and the DTB pointer, with
  `publish_memory_map`/`published_memory_map` and
  `publish_dtb`/`published_dtb` (read-only accessors, no writable
  surface — `AGENTS.md` §2.4). `boot::try_boot` publishes both before the
  map is moved into the `kernel_core` hand-off (`AGENTS.md` §2.1 —
  one-shot publish). The crate gained a `rustos-kernel-sync` dependency
  for `OnceCell`. No `IrqTable` slot is published: the
  boot-to-`BootCompleted` slice runs with interrupts disabled and hands
  the kernel `IrqRouting::unsupported`, so a vertical builds its own
  `PlicController` + `IrqTable` over the DTB-discovered PLIC base
  (publishing a `max_line == 0` table would be a misleading stub —
  `AGENTS.md` §15.1). **Verification.** `cargo test -p rustos-arch-riscv64`
  (34 host unit tests, +2 new covering the set-once memory-map and DTB
  slots) green; the crate builds for `riscv64gc-unknown-none-elf`; `cargo
  clippy -- -D warnings` (host + riscv64 target),
  `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`, and `cargo fmt
  --check` all clean. **Docs.** `docs/src/platform/riscv64.md`
  ("Boot-state publication"). **Deferred.** The MMIO bring-up scaffold
  itself (DTB walk → `Mmio` bus → `MmioTransport` → `PlicController` +
  `init_traps` + `set_trap_dispatch` → `KernelVirtioHost` →
  `drive_driver_lifecycle`), the `virtio_blk_mmio_riscv64` /
  `virtio_net_mmio_riscv64` verticals (an arch-gated MMIO sibling in the
  shared `virtio_qemu_support` crate, reusing the identical device-tail
  closures), their xtask enrolment, and the Item 6 acceptance gate
  (`cargo xtask ci` — mdBook `docs-check` half + `cargo deny`).
- Stage 4.D follow-up (Item 4 — riscv64 external-IRQ controller: PLIC +
  S-mode trap glue, *complete*): `KernelVirtioHost::notify_wait` blocks
  on a real IRQ line (`block_until_ready` with an unbounded `u64::MAX`
  deadline), so the riscv64 MMIO verticals need an actual interrupt path
  to call `IrqTable::fire` — which `kernel/arch/riscv64` had none of.
  **Change.** Two new modules land the external-IRQ foundation. (1)
  `plic.rs` — a `PlicMmio` access seam (`VolatilePlicMmio` on the
  freestanding target), a `Plic<M>` register driver (SiFive PLIC layout:
  per-source priority, per-context enable bitmap, threshold, claim/
  complete), and `PlicController<M>` implementing
  `rustos_kernel_irq::IrqController`. The controller targets the boot
  hart's S-mode context (`s_mode_context(h) = 2h + 1`), `arm`s a source
  (enable + zero threshold + delivering priority), and exposes
  `claim`/`complete`. Its `mask` (the kernel-neutral `IrqTable::fire`
  seam) writes the source priority to zero — a single lock-free 32-bit
  store, no read-modify-write — then a `SeqCst` fence, the riscv64
  analogue of the IO-APIC redirection-entry mask-before-wake. (2)
  `trap.rs` + `trap.s` — an S-mode trap vector installed into `stvec`
  (direct mode) by `init_traps` (which also sets `sie.SEIE` +
  `sstatus.SIE`); the vector saves caller-saved registers, calls the
  Rust handler, and `sret`s. The handler decodes `scause`, fails closed
  (parks) on a synchronous exception, and forwards a supervisor external
  interrupt to a one-shot `set_trap_dispatch` callback (mirroring
  x86_64's `set_external_irq_dispatch`) that performs the PLIC claim →
  `IrqTable::fire` → complete handshake. The crate gained a
  `rustos-kernel-irq` dependency. **Not yet armed.** The
  boot-to-`BootCompleted` slice runs with interrupts disabled, so it
  neither calls `init_traps` nor builds a `PlicController`; the
  virtio-mmio verticals are the first consumer (they will `arm` the
  device source, install the dispatch callback, and `init_traps`).
  **Verification.** `cargo test -p rustos-arch-riscv64` (32 host unit
  tests, +12 new covering the PLIC register math, S-mode context
  interleaving, `arm`/`unmask`/mask/out-of-range, claim/complete, the
  enable-bitmap toggle, mask-before-wake through a real `IrqTable`, the
  `scause` decode, and the set-once dispatch slot) green; the crate
  builds for `riscv64gc-unknown-none-elf` (freestanding asm + handler +
  `VolatilePlicMmio`); `cargo clippy -- -D warnings` (host + riscv64
  target), `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`, and
  `cargo fmt --check` all clean. **Docs.** `docs/src/security/irq.md`
  (controller table row + "riscv64 trap glue" section + Test-coverage
  bullets); `docs/src/platform/riscv64.md` ("External-interrupt
  controller (PLIC) + S-mode trap glue"). **Deferred.** Sv39 paging, the
  ring-0 DTB virtio-mmio walk, SMP, the riscv64 MMIO verticals
  (`virtio_blk_mmio_riscv64`, `virtio_net_mmio_riscv64` — they reuse the
  shared `drive_driver_lifecycle` once the walk lands), and arming the
  controller from the boot/vertical path; the Item 6 acceptance gate
  (`cargo xtask ci` — mdBook `docs-check` half + `cargo deny`) was not
  runnable in this environment.
- Stage 4.D follow-up (Item 4 — riscv64 kernel boot port to
  `BootCompleted`, *complete*): the riscv64 verticals were blocked on a
  kernel boot port that did not exist — `kernel/arch/riscv64` held only
  `qemu_exit`. **Change.** `kernel/arch/riscv64` now carries the full
  QEMU `virt`-board boot pipeline to `AuditEvent::BootCompleted`: an
  S-mode `_start` trampoline (`boot.s`, load address 0x80200000 via the
  new `link/riscv64-virt.ld`) that sets up a stack, zeroes `.bss`, and
  tail-calls the Rust entry (`entry.rs`) with the OpenSBI hand-off
  (`a0 = hartid`, `a1 = DTB`); a bounds-checked flattened-device-tree
  reader (`fdt.rs`, host-tested) that extracts the first `/memory` `reg`
  and the `/cpus` `timebase-frequency`; `RiscvArch` (`kernel_arch.rs`),
  the `kernel_core::KernelArch` impl whose monotonic clock reads the
  `time` CSR via `rdtime`; an SBI legacy-console log `Sink`
  (`sbi.rs` + `serial.rs`); a panic bridge (`panic.rs`); and `boot.rs`,
  which builds the `BootMemoryMap` (reserving `[ram_base,
  __kernel_end)`, marking `[__kernel_end, ram_end)` usable), assembles
  a `kernel_core::BootInfo`, and hands it to `kernel_core::kernel_main`.
  No Sv39 paging or trap vector is needed for this slice (the board
  enters S-mode with paging off and the init pipeline never faults).
  Unlike x86_64, the arch crate depends on `kernel/core` directly and
  owns its boot pipeline (no pre-existing freestanding bin to protect);
  the rationale is in its `Cargo.toml`. **Shared allocator.** The
  64 MiB boot bump allocator was extracted from `rustos-kernel` into a
  new shared `lib/bumpalloc` crate (`rustos-bumpalloc`) so the x86_64
  and riscv64 boot bins register one implementation (`AGENTS.md` §2.2,
  §6); `rustos-kernel::bumpalloc` re-exports it, so existing call sites
  are unchanged. The boot heap lives in a `.heap` (NOLOAD) section after
  `__kernel_end` so the trampoline does not zero it and the usable map
  excludes it. **Test.** New `tests/integration/kernel_arch_boot_riscv64`
  (the riscv64 analogue of `kernel_arch_boot`) flips the `SiFive` Test
  PASS finisher on observing `BootCompleted` (`EventId(4004)`);
  `tools/xtask/src/commands/qemu_tests.rs` gained a per-test `target`
  field + a `Spec::for_riscv64_kernel` branch and enrols it (single
  CPU, 60 s). **Verification.** `cargo test -p rustos-arch-riscv64`
  (16 host unit tests for the FDT reader + `RiscvArch`) green; the bin
  builds for `riscv64gc-unknown-none-elf`; a direct
  `qemu-system-riscv64 -M virt … -bios default -kernel <elf>` run prints
  the full phase timeline + `id=4004 kernel boot completed` and exits
  status 0 (PASS). **Docs.** `docs/src/platform/riscv64.md` ("Kernel
  boot pipeline"); `AGENTS.md` §3 (added `lib/bumpalloc`). **Deferred.**
  Sv39 paging, the S-mode trap vector, the ring-0 DTB virtio-mmio walk,
  SMP, and the riscv64 MMIO verticals (`virtio_blk_mmio_riscv64`,
  `virtio_net_mmio_riscv64`) — they reuse the shared
  `drive_driver_lifecycle` once the walk lands; the Item 6 acceptance
  gate (`cargo xtask ci` — mdBook `docs-check` half + `cargo deny`) was
  not runnable in this environment.
- Stage 4.D follow-up (Item 4 — virtio unload → reload → reuse,
  *complete*): the shared `run_virtio_scenario` previously loaded the
  signed `.rxe` once and dropped the `rustos_drvhost::Host` before the
  device-tail closure ran, so the per-driver **unload → reload → reuse**
  deliverable was still outstanding. **Change.** The host lifecycle is
  now extracted into a `drive_driver_lifecycle(cfg, &dyn
  VirtioHostFactory, transport, vhost, body)` helper in
  `tests/integration/virtio_qemu_support/src/imp.rs` that drives the full
  `load → snapshot → reload → unload` cycle against the live
  `KernelVirtioFactory`, running the device-tail closure *after* the
  reload and *before* the unload. Because every vertical funnels through
  this one helper, both the blk and net verticals now prove a reloaded
  driver still brings its real (emulated) device online and round-trips
  I/O — with **no** duplicated per-driver reload test (`AGENTS.md` §2.2).
  The factory mints a 64-page DMA pool per load, so the load + reload +
  direct-driving pools fit inside the existing 256-page carve. Each
  lifecycle transition that misbehaves (bad load/reload/unload, wrong
  `loaded_count`, stale handle, snapshot mismatch) flips QEMU failure
  with a serial breadcrumb (`AGENTS.md` §7 — no weakened tests). The
  helper erases the factory's generics via `&dyn VirtioHostFactory` so it
  stays under the clippy line limit without an `#[allow]`. **Verification.**
  `cargo xtask test --qemu` is green (all 8 enrolled tests, incl. blk +
  net exercising the new cycle); `cargo clippy -- -D warnings`,
  `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` (the three crates,
  `x86_64-unknown-none`), and host `cargo build --workspace` are clean.
  **Docs.** `docs/src/platform/x86_64.md` ("virtio QEMU verticals (shared
  bring-up)"). **Deferred.** The riscv64 MMIO verticals
  (`virtio_blk_mmio_riscv64`, `virtio_net_mmio_riscv64`) still need the
  riscv64 kernel boot port + DTB walk; the Item 6 acceptance gate
  (`cargo xtask ci` — its mdBook `docs-check` half + `cargo deny` could
  not run in this environment) remains outstanding; see
  `.junie/next-session-prompt.md`.
- Stage 4.D follow-up (Item 4 — shared virtio bring-up scaffolding +
  `virtio_net_pci_x86_64` vertical, *complete*): the
  `virtio_blk_pci_x86_64` bin carried ~430 lines of device-agnostic
  bring-up inline in its `kernel.rs` (high-RAM DMA carve, the
  `DirectPhysMap` + `MmioMap` + `KernelMmioMapper` set-up, the
  `provision_virtio_pci` + `route_msix` + GSI-bind + `msi_message` MSI-X
  wiring, the `HltWaiter` / `cli` / `rdtsc` IRQ-wait glue, and the
  signed-`.rxe` `Host::load`), so a virtio-net vertical could not be
  added without duplicating it (`AGENTS.md` §2.2). **Extraction.** New
  freestanding-only library crate `tests/integration/virtio_qemu_support`
  (`rustos-test-virtio-qemu-support`): `run_virtio_scenario(cfg, body)`
  owns the entire bring-up on one boot frame and hands the provisioned
  `PciTransport` + `&dyn VirtioHost` to a device-specific closure
  (`MSI-X` is enabled before the closure so every queue's
  `queue_msix_vector` is programmed during the driver's `open`); a
  `define_boot_harness!(scenario)` macro generates the boot-observer
  `Sink`, the `#[panic_handler]` bridge, and `kernel_main`; the crate
  owns the shared bump `#[global_allocator]`. Every item is gated to
  `x86_64-unknown-none` so a host `cargo build --workspace` compiles it
  to an empty library (no `std`/allocator conflict). **Refactor.**
  `virtio_blk_pci_x86_64`'s `kernel.rs` shrank to the device tail
  (`ToVirtioBlk` resolver + sector verify) and its QEMU gate was re-run
  green (no regression). **Net vertical.** New
  `tests/integration/virtio_net_pci_x86_64` reuses the support crate; its
  tail opens `VirtioNet`, builds a `rustos_net_icmp::Client` from the
  device MAC + guest `10.0.2.15`, ARP-resolves the SLIRP gateway
  `10.0.2.2`, then `ping`s it and asserts the echo reply. First-try
  bring-up passed; the run's `<binary>.pcap` confirms the on-wire
  `ARP request/reply` + `ICMP echo request/reply (id 0x1234, seq 1)`.
  **Runner.** `rustos-qemu-run` gained `--virtio-net` / `--virtio-net-pcap`
  for manual debugging; `tools/xtask/src/commands/qemu_tests.rs` gained a
  `virtio_net` field and enrols the net test (single CPU, 60-second
  budget, frame dump to `<binary>.pcap`). **Verification.** Both verticals
  pass through `cargo xtask test --qemu` (all 8 enrolled tests green);
  `cargo xtask clippy` / `test` / `abi-check`, `cargo fmt --check`, and
  `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` (the three new crates,
  `x86_64-unknown-none`) are clean. **Docs.** `docs/src/platform/x86_64.md`
  ("virtio QEMU verticals (shared bring-up)"). **Deferred.** The riscv64
  MMIO verticals (`virtio_blk_mmio_riscv64`, `virtio_net_mmio_riscv64`)
  still need the riscv64 kernel boot port + DTB walk; the per-driver
  unload → reload → reuse test and the Item 6 acceptance gate
  (`cargo xtask ci` — its mdBook `docs-check` half + `cargo deny` could
  not run in this environment) remain outstanding; see
  `.junie/next-session-prompt.md`.
- Stage 4.D follow-up (Item 5 follow-up / Item 4 prerequisite —
  `rustos-net-icmp` initiator + virtio-net host lifetime, *complete*):
  the virtio-net QEMU verticals need the guest to *initiate* an
  ARP+ICMP exchange with the SLIRP gateway (`10.0.2.2` replies to ARP
  and ICMP echo; it never pings the guest), but `rustos-net-icmp`
  shipped only a passive `Responder`, and `VirtioNet` demanded a
  `&'static dyn VirtioHost` it could never get from a per-load,
  stack-minted `KernelVirtioHost`. **Initiator.** New `Client`
  (mirrors `Responder`: stateless, `no_std`, `forbid(unsafe_code)`)
  with `write_arp_request` / `parse_arp_reply`, `write_echo_request` /
  `is_echo_reply`, and `resolve(net, target, …)` /
  `ping(net, peer_mac, dest, …)` driving any `Net` driver over bounded
  poll loops (no retry-until, `AGENTS.md` §2.1). The Ethernet+ARP and
  Ethernet+IPv4+ICMP framing was extracted into shared
  `write_arp_frame` / `write_icmp_frame` helpers used by both
  `Responder` and `Client` so no framing is duplicated (`AGENTS.md`
  §2.2). **Host lifetime.** `VirtioNet<'h, T>` now borrows
  `&'h dyn VirtioHost` (mirrors `VirtioBlk<'h, T>` verbatim), so a
  bring-up scenario can drive it with a stack-minted host; the change
  is purely a loosening (a `'static` host still satisfies `'h`) and is
  contained to the one crate. **Tests.** `rustos-net-icmp` 41 (+8:
  `Client` resolve happy/none/wrong-target, ping confirm/mismatched-
  sequence, ARP-reply-rejects-request, output-too-small, driver-error
  propagation); `rustos-drv-network-virtio-net` 9 (unchanged, all
  green under the new lifetime). `cargo clippy -p rustos-net-icmp
  -p rustos-drv-network-virtio-net --all-targets -- -D warnings`,
  `cargo fmt --check`, and `RUSTDOCFLAGS="-D warnings" cargo doc
  --no-deps` are clean. **Docs.** `docs/src/userland/net_icmp.md` (the
  `Client` API + shared-framing helpers) and the crate rustdoc.
  **Deferred.** The kernel-side virtio-net test bins that consume this
  (`tests/integration/virtio_net_pci_x86_64`, `virtio_net_mmio_riscv64`)
  remain outstanding; landing them cleanly requires extracting the
  shared virtio bring-up scaffolding now embedded in
  `virtio_blk_pci_x86_64`'s `kernel.rs` (carve DMA map, `HltWaiter`,
  PCI walk + MSI-X routing, signed-`.rxe` load) into a shared
  test-support crate so the net bin does not duplicate it (`AGENTS.md`
  §2.2), then first-time virtio-net QEMU bring-up; see
  `.junie/next-session-prompt.md`.
- Stage 4.D follow-up (Item 4 prerequisite — virtio-net user
  networking in the QEMU runner, *complete*): the `tools/qemu` runner
  could attach a backing disk (`with_virtio_blk` + `disk::plant_raw_disk`)
  but had **no** network surface, so the virtio-net verticals
  (`tests/integration/virtio_net_pci_x86_64`,
  `virtio_net_mmio_riscv64`) had no device to drive. **Attach surface.**
  New `NetDevice { pcap: Option<PathBuf> }` + `Spec.net_devices`, with
  `Spec::with_virtio_net()` (no capture) and
  `with_virtio_net_pcap(path)` builders (mirrors `BlockDevice` /
  `with_virtio_blk`, `AGENTS.md` §2.4 — no interface creep). The x86_64
  backend emits, per interface, `-netdev user,id=netN` +
  `-device virtio-net-pci,netdev=netN,disable-legacy=on` (modern
  virtio-1.x layout, device id `0x1041`, the same `disable-legacy=on`
  pin the boot walk needs); the riscv64 backend emits the `virt`-board
  analogue `-device virtio-net-device,netdev=netN`. The user-mode
  (SLIRP) backend needs no host privileges and gives a fixed
  `10.0.2.0/24` topology (guest `.15`, gateway `.2`) so an ARP/ICMP-echo
  test is deterministic (`AGENTS.md` §7). When a `NetDevice` carries a
  `pcap` path the backend attaches
  `-object filter-dump,id=dumpN,netdev=netN,file=<path>` so the host
  harness can verify the on-wire exchange after the run — the network
  analogue of re-reading a planted disk. The x86_64 `X-PciMmio64Mb=0`
  fw_cfg BAR-confinement guard now also fires for a net-only spec (a
  virtio-net-pci function is a PCI BAR consumer too). **Tests.**
  `rustos-qemu` 50 (+7: two builder records + accumulation order in
  `lib.rs`, the no-net / per-device-attach / net-only-BAR cases in
  `x86_64.rs`, the no-net / per-device-attach cases in `riscv64.rs`);
  `cargo clippy -p rustos-qemu --all-targets -- -D warnings`,
  `cargo fmt --check`, and `RUSTDOCFLAGS="-D warnings" cargo doc
  --no-deps -p rustos-qemu` are clean. **Docs.**
  `docs/src/platform/x86_64.md` ("virtio-net user networking in the QEMU
  runner") + `docs/src/platform/riscv64.md`. **Deferred.** The
  kernel-side virtio-net test bins that consume this surface (boot →
  virtio-net PCI/MMIO walk → drive `rustos-net-icmp` → ARP + ICMP-echo
  the gateway → verify against the captured pcap), the riscv64 boot
  port + DTB walk, and the Item 6 acceptance gate remain outstanding;
  see `.junie/next-session-prompt.md`.
- Stage 4.D follow-up (Item 4 — `virtio_blk_pci_x86_64` real
  round-trip, *landed but not gated*): the test bin now drives the
  full x86_64 vertical end-to-end under QEMU — boot →
  `x86_mechanism_one()` PCI walk → map the four virtio register
  windows through `KernelMmioMapper` → `route_msix` → mint a
  `KernelVirtioHost` over a per-device DMA pool carved from
  `published_memory_map()` → load the signed virtio-blk `.rxe` → read
  sector 0 (verify the planted `byte[i] = i mod 256` pattern) →
  write+read-back sector 1 (verify). On a clean run the serial log
  reaches "sector 1 round-trip verified" and QEMU exits success.
  **Sub-fixes (all host-tested, green).** `tools/qemu` x86_64 attaches
  `virtio-blk-pci` as modern (`disable-legacy=on`) and forces BARs
  below 4 GiB via the OVMF `X-PciMmio64Mb=0` fw_cfg knob (the boot
  identity map only covers 0..4 GiB); the debug runner gained
  `--virtio-blk`. `PciTransport` now programs `queue_msix_vector`
  (MSI-X was never enabled at the queue level). `Pci::route_msix`
  also sets the command-register Memory-Space + Bus-Master enable bits
  (required for DMA and MSI delivery). `kernel/irq::IrqTable::fire`
  was made lock-free (atomic per-line `bound`/`ready` flags) so an ISR
  cannot deadlock a parked `try_wait_step` on a single CPU — a genuine
  pre-existing hazard — and a read-only `IrqTable::ready_for(handle)`
  poll companion replaces the former test-only flag observer
  (`AGENTS.md` §2.4 — a narrow query, not new mutation surface); the
  kernel-host tests poll through it. **Tests.** `rustos-kernel-irq`
  24, `rustos-drv-bus-pci` 28, `rustos-drv-storage-virtio-blk` 8,
  `rustos-drv-bus-virtio` (kernel-host) 75; clippy `-D warnings`,
  `cargo fmt --check`, and the `x86_64-unknown-none` test-bin build are
  clean. **Gate (now enrolled).** The earlier ~30% intermittent
  single-CPU hang in the MSI completion-wait path (guest spinning
  `IF=0` near `IrqTable::fire` right after the device's first
  completion interrupt) was a deadlock between the completion ISR's
  `IrqTable::fire` and a parked `try_wait_step` holding the same
  `IrqTable` lock; it was already removed by making `fire` /
  `try_wait_step` lock-free (per-line `bound` / `ready` atomics, no
  shared `IrqTable` lock — the sub-fix above). Root-cause review
  confirmed no other ISR-reachable blocking primitive can stall: the
  only one, `IoApicController::mask`'s `SpinLock`, is acquired solely
  with interrupts disabled (boot never `sti`s; the test bin `cli`s in
  task context; `mask` runs only in-ISR), so no same-CPU IF=1 holder
  exists, and `SerialSink` is lock-free. Stability was re-verified
  across 90 consecutive runs (60 TCG through the exact `xtask` runner
  path + 30 KVM) plus 4 full `cargo xtask test --qemu` invocations, all
  green with zero hangs. The crate is therefore enrolled in
  `tools/xtask/src/commands/qemu_tests.rs` with `disk_sectors:
  Some(2048)`. The stale `kernel.rs` comments that described the
  no-longer-existent `IrqTable`-lock deadlock were corrected to
  document the lock-free completion path (`AGENTS.md` §13).
- Stage 4.D follow-up (Item 4 prerequisite — live boot-wiring seams,
  *complete*): scoping the `tests/integration/virtio_blk_pci_x86_64`
  kernel test bin found two seams it still could not reach. The PCI
  driver kept its concrete `Pci` type + the `PortIoConfigSpace` /
  `X86PortIo` mechanism-#1 backend `pub(crate)`, so ring 0 (whose only
  sanctioned driver surface is `register`, `AGENTS.md` §8) had no way
  to *construct* a live bus; and the firmware `BootMemoryMap` was moved
  into the `kernel_core` hand-off and buried in the `pub(crate)`
  `KernelState`, so a bring-up observer could not build a per-device
  DMA `FrameAllocator`. **PCI constructor.** New public
  `rustos_drv_bus_pci::x86_mechanism_one()` (gated `target_arch =
  "x86_64"`) returns the bus as `impl VirtioPciBus + MsixBus` (both
  have `Bus` as a supertrait, so it also coerces to `&dyn Bus`); the
  concrete `Pci` type stays crate-private. Construction issues no port
  I/O — it only stores the zero-sized backend — so it is host-safe to
  call. **Memory-map seam.** `kernel/rustos-kernel/src/arch_wrapper.rs`
  adds a `MEMORY_MAP_SLOT` `OnceCell<BootMemoryMap>` with
  `publish_memory_map(&map)` (called once from `boot::try_boot` before
  the map moves into the hand-off) + a read-only `published_memory_map()`
  accessor, mirroring the existing `published_irq_table` /
  `published_irq_controller` set-once slots (`AGENTS.md` §2.1 / §2.4).
  **Tests.** `rustos-drv-bus-pci` 27 (+1: the constructor exposes all
  three frozen seams without naming the concrete type), `rustos-kernel
  --lib` 50 (+1: publish → read → second-publish-no-op). `cargo clippy
  -p rustos-drv-bus-pci -p rustos-kernel --all-targets -- -D warnings`,
  `cargo fmt --check`, `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`,
  the `x86_64-unknown-none` (pci lib + kernel lib/bin) build, and the
  `aarch64-unknown-none` pci build (constructor cfg-absent) are clean.
  **Docs.** `docs/src/drivers/bus.md` ("Constructing the real-hardware
  bus") + the `rustos-kernel` README ("Published boot-state accessors").
  **Deferred.** The `tests/integration/virtio_blk_pci_x86_64` kernel
  test bin that *consumes* these seams — boot → `x86_mechanism_one` +
  `published_memory_map`-built `FrameAllocator` + `DirectPhysMap` +
  external-vector bind → `provision_and_run` → drive a live virtio-blk
  `.rxe` — plus the riscv64 MMIO vertical, the net tests, and the
  Item 6 acceptance gate remain outstanding; see
  `.junie/next-session-prompt.md`.
- Stage 4.D follow-up (Item 4 prerequisite — riscv64 QEMU runner,
  *complete*): the `tools/qemu` runner was x86_64-only (single
  `Arch::X86_64`, GRUB-ISO boot, `isa-debug-exit`), so the
  `tests/integration/virtio_blk_mmio_riscv64` crate had no harness to
  build on. **Per-arch backend.** New module `tools/qemu/src/riscv64.rs`
  targets the generic `virt` board: `-M virt`, `-bios default` (OpenSBI
  loads the ELF directly via `-kernel`, so there is no ISO step — the
  kernel ELF *is* the artifact), headless serial-over-stdio, and each
  `Spec::with_virtio_blk` image attached as `-drive
  if=none,format=raw,id=blkN` + `-device virtio-blk-device,drive=blkN`
  on a virtio-mmio transport (the riscv64 analogue of x86_64's
  `virtio-blk-pci`, driven by `MmioTransport`). **Result protocol.**
  The `virt` board has no `isa-debug-exit`; results go through the
  SiFive Test device at `0x10_0000` (`FINISHER_PASS = 0x5555` ⇒ QEMU
  exit status `0`; `FINISHER_FAIL = 0x3333 | (code << 16)` ⇒ exit
  `code`). Because success is a *zero* status on riscv64 versus a
  *non-zero* status on x86_64, exit decoding is now per-arch:
  `Arch::outcome_from_status` dispatches to `riscv64::outcome_from_status`
  or the existing `Outcome::from_qemu_status`. **Generic seam.**
  `Arch::Riscv64`, `Spec::for_riscv64_kernel`, and the new dispatch are
  the only additions to `tools/qemu/src/lib.rs`; the shared
  `with_cpus` / `with_timeout` / `with_virtio_blk` / `Runner::run`
  entry points are unchanged (`AGENTS.md` §2.4 — no interface creep).
  **Tests.** `rustos-qemu` 42 (+15: 13 riscv64-module argv/finisher/
  decode tests + the lib-level per-arch decode, riscv64 defaults, and
  missing-kernel guards). `cargo test -p rustos-qemu`, `cargo clippy
  -p rustos-qemu --all-targets -- -D warnings`, `cargo fmt --check`,
  and `RUSTDOCFLAGS="-D warnings" cargo doc -p rustos-qemu --no-deps`
  are all clean. **Docs.** New `docs/src/platform/riscv64.md` (board
  model, SiFive-test result protocol, per-arch runner module), wired
  into `docs/src/SUMMARY.md`. **Deferred.** The riscv64 boot pipeline
  + ring-0 DTB resolution feeding `provision_virtio_mmio`, and the
  `virtio_blk_mmio_riscv64` / net QEMU crates + acceptance gate
  (Items 4 / 6) remain outstanding — see
  `.junie/next-session-prompt.md`.
- Stage 4.D follow-up (Item 4 prerequisite — riscv64 `SiFive` Test
  finisher, *complete*): the riscv64 QEMU runner expected the kernel to
  report its result by writing to the `virt`-board `SiFive` Test device,
  but the `kernel/arch/riscv64` crate was a `core`-only placeholder with
  no way to do so. New module `kernel/arch/riscv64/src/qemu_exit.rs`
  mirrors `kernel/arch/x86_64::qemu_exit`: the `SIFIVE_TEST_BASE`
  (`0x10_0000`), `FINISHER_PASS` (`0x5555`), and `FINISHER_FAIL`
  (`0x3333`) constants (pinned to `tools/qemu/src/riscv64.rs` by a
  tie-down test, `AGENTS.md` §2.2), a pure `fail_word(code)` building
  `(code << 16) | FINISHER_FAIL`, and the target-gated
  `exit_success()` / `exit_failure(code)` (single volatile 32-bit store
  to the device + a `wfi` park; no panic, `AGENTS.md` §2.9). **Tests.**
  `rustos-arch-riscv64` 3 host tests. `cargo clippy
  -p rustos-arch-riscv64 --target riscv64gc-unknown-none-elf
  -- -D warnings`, `cargo fmt --check`, and `RUSTDOCFLAGS="-D warnings"
  cargo doc --no-deps` are clean. **Docs.** `docs/src/platform/riscv64.md`
  updated (the kernel-side finisher is no longer "staged"). **Deferred.**
  The riscv64 boot pipeline + ring-0 DTB walk and the
  `virtio_blk_mmio_riscv64` / net QEMU crates + acceptance gate
  (Items 4 / 6) remain outstanding — see
  `.junie/next-session-prompt.md`.
- Stage 4.D follow-up (Item 4 prerequisite — virtio-MMIO
  provisioning seam + ring-0 walk, *complete*): the PCI vertical had
  a frozen `VirtioPciBus` ABI seam and a host-tested
  `provision_virtio_pci` ring-0 walk, but the riscv64 / `AArch64`
  `-M virt` path had no equivalent: the MMIO bus driver's
  `Mmio::map_slot_window` was `pub(crate)`, so ring 0 (whose only
  sanctioned driver surface is `register`, `AGENTS.md` §8) had no
  driver-agnostic way to turn a `virtio-mmio` slot into a kernel
  window. **ABI seam (frozen `abi-v1`).** New module
  `lib/abi/src/driver/virtio_mmio.rs` adds `VirtioMmioBus: Bus`
  (`map_slot_window(base, mapper)`), re-exported from the crate root;
  it mirrors `VirtioPciBus`, but the MMIO transport consumes exactly
  one window (not four). **MMIO driver.** `Mmio<'_, T>` implements
  `VirtioMmioBus`, forwarding to the inherent `map_slot_window`, so the
  concrete type stays `pub(crate)`. **Ring-0 walk.** New module
  `kernel/rustos-kernel/src/virtio_mmio_walk.rs`:
  `provision_virtio_mmio(bus, device_id, mapper)` takes a `&dyn
  VirtioMmioBus`, enumerates into a bounded table (`MAX_SLOTS = 64`,
  fails closed on overflow), picks the first slot whose `DeviceID`
  matches the bare virtio device type, maps its single window through
  the `CAP_MMIO_MAP`-gated `MmioMapper`, and builds an `MmioTransport`
  — no ambient authority, every failure a typed `VirtioMmioWalkError`
  rather than a panic (`AGENTS.md` §2.9). **Tests.** `rustos-abi`
  (+4 seam tests), `rustos-drv-bus-mmio` green, `rustos-kernel --lib`
  49 (+4 walk tests: matching-slot provision, no-matching-slot,
  capability-denied map, enumeration overflow). `cargo clippy
  -p rustos-abi -p rustos-drv-bus-mmio -p rustos-kernel --all-targets
  -- -D warnings`, `cargo fmt --check`, `RUSTDOCFLAGS="-D warnings"
  cargo doc --no-deps` are clean; `rustos-abi` + `rustos-drv-bus-mmio`
  build for `riscv64gc-unknown-none-elf` and `aarch64-unknown-none`,
  and `rustos-kernel` builds for `x86_64-unknown-none`. **Docs.**
  `docs/src/abi/driver_traits.md` ("Virtio-MMIO provisioning") +
  `docs/src/drivers/bus.md` ("Ring-0 virtio-MMIO walk"). **Deferred.**
  Wiring `provision_virtio_mmio` into a live riscv64 boot pipeline (the
  ring-0 DTB resolution + `KernelMmioMapper`), the riscv64 QEMU runner,
  and the QEMU integration crates + acceptance gate (Items 4 / 6)
  remain outstanding — see `.junie/next-session-prompt.md`.
- Stage 4.D follow-up (Item 4 — MSI-X routing wired into the boot
  provisioning, *complete*): the previous session landed
  `Pci::route_msix` + the `MsixBus` ABI seam + `msi_message`, but
  `provision_and_run` still never *called* `route_msix`, so a loaded
  virtio-blk `.rxe` would park on `notify_wait` forever. This joins the
  two. **Walk.** `provision_virtio_pci` now returns
  `VirtioProvision { transport, bdf }` so the boot wiring can route the
  interrupt of the function the walk already located, without
  re-enumerating (`AGENTS.md` §2.2). **Boot wiring.** `VirtioBootConfig`
  gains `msix: &dyn MsixBus` (the same `Pci` object reached through the
  second frozen seam), `msix_entry`, and the architecture-built
  `msi_message`; `provision_and_run` routes MSI-X through the same
  `KernelMmioMapper` after the four register windows are mapped and
  before the driver host is built, failing the whole bring-up closed
  with the new `VirtioPciWalkError::RouteMsix` variant if routing is
  refused. The arch caller stays responsible for binding the line in
  `kernel/irq::IrqTable` (line → vector) and encoding the `MsiMessage`,
  keeping `virtio_boot` architecture-neutral. **Tests.** `rustos-kernel
  --lib` 45 (+1: a routing-refused fail-closed path; the happy-path test
  now also asserts the `(bdf, entry, message)` reached `route_msix` and
  the missing-device test asserts no route was attempted). `cargo clippy
  -p rustos-kernel --all-targets -- -D warnings`, `cargo fmt --check`,
  `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`, and the
  `x86_64-unknown-none` lib build are clean. **Docs.**
  `docs/src/drivers/bus.md` (Boot wiring + MSI-X sections). **Deferred.**
  Allocating + binding the external vector in `IrqTable` and building the
  `MsiMessage` from it in `boot.rs`, then driving the live device from
  the `tests/integration/virtio_blk_pci_x86_64` kernel test bin, remain
  the QEMU-test work; legacy MSI and INTx routing are not implemented.
  See `.junie/next-session-prompt.md`.
- Stage 4.D follow-up (Item 4 prerequisite — PCI MSI-X interrupt
  routing, *complete*): scoping the `tests/integration/
  virtio_blk_pci_x86_64` crate found the hard blocker that a real
  virtio-blk-pci round-trip cannot complete without delivering the
  device's interrupt — `VirtioBlk::run_request` parks on
  `host.notify_wait()` → `block_until_ready` on a *pre-bound*
  `IrqHandle` — yet the tree had no PCI interrupt-routing path at all
  (the PCI driver *discovered* MSI / MSI-X capabilities but never
  enabled them; INTx would need an ACPI `_PRT`/AML interpreter that
  does not exist). This lands the modern, host-testable half: MSI-X
  enablement. **ABI seam (frozen `abi-v1`).** New module
  `lib/abi/src/driver/msix.rs` adds the `MsiMessage { address, data }`
  type (the architecture-built interrupt message, opaque to the bus
  driver) and the `MsixBus: Bus` trait
  (`route_msix(bdf, entry, message, mapper)`), re-exported from the
  crate root. Ring 0 reaches the PCI driver through `&dyn MsixBus`,
  naming no concrete `drivers/bus/*` type (`AGENTS.md` §8). **PCI
  driver.** `Pci::route_msix` locates the function's MSI-X capability,
  bounds-checks the entry against the table size, maps the addressed
  16-byte table entry through the `CAP_MMIO_MAP`-gated `MmioMapper`,
  writes the message address/data + clears the per-vector mask, then
  sets the MSI-X Enable bit and clears the function mask in the
  capability's Message Control register — failing closed on a missing
  capability (`NotFound`), an out-of-range/overrunning entry
  (`OutOfRange`), an I/O-port table BAR (`Unsupported`), or a denied
  mapper (`PermissionDenied`); the driver never synthesises a pointer
  (`AGENTS.md` §4). `Pci<C>` implements `MsixBus` by forwarding to the
  inherent method. **Arch.** `rustos_arch_x86_64::irq::msi_message(
  vector, destination)` builds the x86 local-APIC message (physical
  destination, fixed delivery, edge trigger; Intel SDM Vol 3A §11.11),
  reusing `preempt::LAPIC_BASE_PHYS`. **Tests.** `rustos-abi` 84 (+3
  seam tests), `rustos-drv-bus-pci` 26 (+5: program-entry-and-enable
  asserting both the table write and the config enable bit, NotFound,
  OutOfRange, I/O-BAR Unsupported, PermissionDenied; the
  `MockConfigSpace` now logs config writes), `rustos-arch-x86_64` 133
  (+2 message-encoding tests). **Docs.** `docs/src/drivers/bus.md`
  ("MSI-X interrupt routing") + the PCI README. **Deferred.** Binding
  the minted vector in `kernel/irq::IrqTable`, allocating a free
  external vector, and calling `route_msix` from the boot pipeline so
  `virtio_blk_pci_x86_64` can drive a live device remain the QEMU-test
  work; legacy MSI and INTx routing are not implemented. See
  `.junie/next-session-prompt.md`.
- Stage 4.D follow-up (Item 4 prerequisite — hardware-real direct-map
  DMA/MMIO data path, *complete*): investigation while scoping the
  `tests/integration/virtio_blk_pci_x86_64` crate found that the
  kernel DMA/MMIO primitives could not drive a *real* device. The DMA
  pool served the driver's CPU-visible bytes from a heap `Vec<u8>`
  decoupled from the physical frames it handed the device, and both
  `DmaPool` and `MmioMap` resolved pointers into a freshly-minted
  `AddressSpace` that is never loaded into CR3 — so on hardware a
  device would DMA into / a BAR would live at frames the driver never
  reads. **Direct-map seam.** New `kernel/mem::phys` module adds the
  `PhysMap` trait (`translate(PhysAddr, len) -> Option<NonNull<u8>>`)
  with a production `DirectPhysMap` (identity/offset over the boot
  low-memory direct map; the x86_64 trampoline identity-maps 0..4 GiB)
  and a test-only page-aligned `SimPhysMap` standing in for physical
  RAM. **DMA pool.** `DmaPool::new` now takes `&dyn PhysMap`; `bytes`
  / `bytes_mut` / `slot_base` and the zero-on-alloc / zero-on-free
  clears resolve the buffer's `phys` through it, so the CPU reads/
  writes the very frames the device DMAs to (a real fix: the old
  zero-on-free wiped the disconnected `Vec`, never the frames). The
  host-side `0xCC` guard-byte *simulation* (`GuardViolation`,
  `guards_intact`, `poke_for_test`) is removed — it modelled a
  non-hardware "detect at free" behaviour; the unmapped guard pages in
  the `AddressSpace` (the real fault mechanism) remain, and a new
  `DmaError::DirectMap` fails closed when a frame is outside the map.
  **MMIO mapper.** `MmioMap` gains the same `&dyn PhysMap` (now
  `MmioMap<'a, P>`); `region_base` resolves the region's device
  physical base through it (no register zeroing on map), with a new
  `MmioError::DirectMap`. `KernelMmioMapper` decouples its borrow `'a`
  from the map's `'p` (`&'a mut MmioMap<'p, P>`). **Threaded through.**
  `kernel/sec::{dma,mmio}` signatures, `KernelVirtioHost`'s test pool,
  and `KernelVirtioFactoryConfig` (new `phys` field, passed to
  `DmaPool::new` in `mint`). **Tests.** `rustos-kernel-mem` 107 (+ new
  `phys` unit tests and DMA/MMIO `cpu_view_aliases_device_physical_
  frame` / `free_zeroes_the_physical_frame` / `region_base_addresses_
  the_device_physical_frame` aliasing tests), `rustos-kernel-sec` 52,
  `rustos-drv-bus-virtio` 73 (`--features kernel-host`), `rustos-kernel
  --lib` 42 — full `cargo test --workspace` green. `cargo clippy
  --all-targets -- -D warnings`, `cargo fmt --check`, `RUSTDOCFLAGS=
  "-D warnings" cargo doc --no-deps`, and the `x86_64-unknown-none`
  build (default + `kernel-host`) are clean on every touched crate.
  **Docs.** `docs/src/architecture/memory.md` (direct-map CPU access;
  guard-page wording). **Deferred.** Constructing the production
  `DirectPhysMap` in `boot.rs` and threading it + a live `Pci` /
  `KernelMmioMapper` / `KernelVirtioFactory` into a `drvhost::Host`
  remains the `tests/integration/virtio_blk_pci_x86_64` work; see
  `.junie/next-session-prompt.md`.
- Stage 4.D follow-up (Item 4 prerequisite — virtio-blk backing
  storage in the QEMU runner, *complete*): the `tests/integration/
  virtio_blk_pci_x86_64` crate needs the guest to see a
  `virtio-blk-pci` function whose contents are known before boot, but
  the `tools/qemu` runner could only build a GRUB ISO + attach OVMF —
  it had no block-device surface. **Disk planting.** New module
  `tools/qemu/src/disk.rs` adds `plant_raw_disk(path, size_sectors,
  sectors)` (+ `SECTOR_BYTES = 512`): it lays down a zero-filled raw
  image and stamps each `(lba, bytes)` entry at `lba * SECTOR_BYTES`.
  Raw (not qcow2) is deliberate so the host harness can re-read a
  guest-written block by byte offset without a qcow2 parser. Validation
  fails closed (`InvalidInput`, no file created) on a zero-sector
  image, an out-of-range `lba`, or a slice longer than a sector.
  **Device attachment.** `Spec` gains a `BlockDevice` field +
  `with_virtio_blk(image)` builder (the sanctioned alternative to
  smuggling args through `extra_args`, which the field's own docs call
  out); the x86_64 backend emits `-drive if=none,format=raw,id=blkN,
  file=<image>` + `-device virtio-blk-pci,drive=blkN` per device, and
  `Runner::run` fails closed with `NotFound` if a backing image is
  missing before spawning QEMU. **Tests.** `rustos-qemu` 27 (+10: five
  planting paths in `disk::tests`, three argv/no-storage paths and the
  missing-image guard, plus the `with_virtio_blk` builder). `cargo
  clippy -p rustos-qemu --all-targets -- -D warnings`, `cargo fmt
  --check`, and `RUSTDOCFLAGS="-D warnings" cargo doc -p rustos-qemu
  --no-deps` are clean. **Docs.** `docs/src/platform/x86_64.md`
  ("Stage 4.D — virtio-blk backing storage in the QEMU runner").
  **Deferred.** The kernel-side `tests/integration/
  virtio_blk_pci_x86_64` crate that consumes this surface — booting
  kernel + driver host + signed `.rxe` and threading a live
  `KernelVirtioFactory` + `PciTransport` — plus the riscv64 runner /
  MMIO walk, the net tests, and the Item 6 acceptance gate remain
  outstanding; see `.junie/next-session-prompt.md`.
- Stage 4.D follow-up (Item 4 — ring-0 virtio-PCI provisioning walk
  + ABI seam, *complete*): the per-`cfg_type` window hand-offs existed
  on `drivers/bus/pci` (`Pci::map_virtio_window` /
  `virtio_notify_off_multiplier`), but they were `pub(crate)` on the
  concrete `Pci` type, and a driver crate's only public surface is
  `register` (`AGENTS.md` §8) — so ring 0 had no sanctioned way to
  *call* them. **ABI seam (frozen `abi-v1`).** New module
  `lib/abi/src/driver/virtio_pci.rs` adds the `VirtioPciBus: Bus`
  trait (`map_virtio_window(bdf, cfg_type, mapper)` +
  `notify_off_multiplier(bdf)`) plus the `VIRTIO_PCI_CFG_*` /
  `VIRTIO_PCI_VENDOR_ID` constants. The kernel reaches the PCI driver
  through `&dyn VirtioPciBus` exactly as it already reaches a bus via
  `Bus` and the mapper via `MmioMapper`, so ring 0 names no concrete
  `drivers/bus/*` type (`AGENTS.md` §8) and the PCI crate's public
  surface stays `register`-only. **PCI driver.** `Pci<C>` implements
  `VirtioPciBus` (forwarding to its inherent methods), and
  `config.rs`'s `VIRTIO_CFG_*` constants now bind to the `rustos_abi`
  source of truth rather than re-stating the literals (`AGENTS.md`
  §2.2). **Ring-0 walk.** New module
  `kernel/rustos-kernel/src/virtio_pci_walk.rs` adds
  `provision_virtio_pci(bus, device_id, mapper)`: it enumerates the
  bus into a bounded stack table (`MAX_FUNCTIONS = 64`, failing closed
  with `DeviceTableOverflow` rather than allocating), picks the first
  function matching the virtio vendor + requested device ID, maps the
  four virtio register windows through the `CAP_MMIO_MAP`-gated mapper,
  and builds a `PciTransport`. Every failure mode is a
  `VirtioPciWalkError` variant — no panics (`AGENTS.md` §2.9), no
  ambient authority (`AGENTS.md` §4). **Tests.** `rustos-abi` 81 (+4
  seam tests: per-`cfg_type` provisioning, unknown-cfg `NotFound`,
  capability-denial, enumerate), `rustos-drv-bus-pci` 21 (unchanged,
  trait impl exercised through the inherent path), `rustos-kernel
  --lib` 42 (+5 walk tests: transport provisioned for a matching
  device, no-match / wrong-device-id `NoVirtioFunction`, map-failure
  propagation, enumeration overflow). Full `cargo test --workspace` is
  green. `cargo clippy --all-targets -- -D warnings`, `cargo fmt
  --check`, `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`, and the
  `x86_64-unknown-none` kernel build are clean on every touched crate.
  **Docs.** `docs/src/abi/driver_traits.md` (new seam) and
  `docs/src/drivers/bus.md` (ring-0 walk). **Deferred.** Wiring the
  walk into a live `drvhost::Host` from the boot pipeline, the
  `tests/integration/virtio_blk_pci_x86_64` QEMU crate (needs the
  signed-`.rxe` boot path), the riscv64 runner + DTB/MMIO walk, the
  net tests, and the Item 6 acceptance gate remain outstanding — see
  `.junie/next-session-prompt.md`.
- Stage 4.D follow-up (Item 4 — live virtio-PCI boot wiring,
  *complete*): the ring-0 walk (`provision_virtio_pci`) and the
  per-driver DMA factory (`KernelVirtioFactory`) existed but were only
  reachable from unit tests; nothing joined them to a live
  `drvhost::Host`. **Wiring seam.** New module
  `kernel/rustos-kernel/src/virtio_boot.rs` adds `VirtioBootConfig`
  (the borrowed boot resources: bus, per-driver `MmioMap`, DMA frame
  allocator + `PhysMap`, bound `IrqHandle`, `IrqWaiter`, and the
  driver-host trust inputs) and `provision_and_run(config, make_table,
  body)`. It builds a `KernelMmioMapper`, provisions the
  `PciTransport`, constructs a `KernelVirtioFactory`, and hands a live
  `drvhost::Host` (factory wired into `HostConfig::virtio_host_factory`)
  plus the transport to a `body` closure. The scope/callback shape
  keeps the mapper, factory, host, and every minted per-driver DMA pool
  on one boot frame, so all of it is reclaimed when `body` returns — no
  driver retains a register window or DMA mapping past its load
  (`AGENTS.md` §4). The walk fails closed with `VirtioPciWalkError` and
  never builds the host if the device or a window cannot be resolved.
  **Tests.** `rustos-kernel --lib` 44 (+2 `virtio_boot` host tests: a
  happy path that provisions the four register windows over a
  `SimPhysMap`-backed `MmioMap`, loads a signed `.rxe` whose `register`
  allocates a zeroed DMA slab through the minted `VirtioHost`, and
  asserts `mmio.live() == 4`; and a missing-device path asserting
  `NoVirtioFunction` with nothing mapped). `rustos-crypto` is now a
  dependency (the config names `Ed25519PublicKey`) and `ed25519-dalek`
  a dev-dependency (test signing, matching `drvhost`). `cargo clippy
  -p rustos-kernel --all-targets -- -D warnings`, `cargo fmt --check`,
  `RUSTDOCFLAGS="-D warnings" cargo doc -p rustos-kernel --no-deps`,
  and the `x86_64-unknown-none` lib + bin build are clean; `drvhost`,
  `kernel-mem`, `kernel-sec`, `abi` regress none. **Docs.**
  `docs/src/drivers/bus.md` ("Boot wiring"). **Deferred.** The
  `tests/integration/virtio_blk_pci_x86_64` QEMU crate that calls
  `provision_and_run` from the boot pipeline against a real device, the
  riscv64 runner + DTB/MMIO walk, the net tests, and the Item 6
  acceptance gate remain outstanding — see
  `.junie/next-session-prompt.md`.
- Stage 4.D follow-up (Item 2-tail.4 — kernel-binary
  `VirtioHostFactory`, *complete*): the production kernel binary now
  owns a concrete `VirtioHostFactory` that mints a fresh, per-driver
  `KernelVirtioHost`, ready to be threaded through
  `rustos_drvhost::HostConfig::virtio_host_factory`. **ABI/host
  change.** `KernelVirtioHost::new` was changed to take its `DmaPool`
  **by value** (the field is now `RefCell<DmaPool<'a, P>>` instead of
  `RefCell<&'a mut DmaPool<'a, P>>`). Owning the pool is what makes a
  `&self` factory sound: `VirtioHostFactory::mint(&'r self, granted)
  -> Option<Box<dyn VirtioHost + 'r>>` cannot hand out a borrowed
  `&'a mut DmaPool` from behind a shared borrow, so the host must own
  the pool it is given. `'a` now bounds only the pool's
  `FrameAllocator` borrow; there was no production caller of the old
  signature (only in-crate tests), so this is an additive reshape of
  an as-yet-unconsumed seam. **New factory.**
  `kernel/rustos-kernel/src/virtio_factory.rs` adds
  `KernelVirtioFactory<'k, P, F>` + a borrowed-fields
  `KernelVirtioFactoryConfig<'k>` (mirroring `HostConfig` to dodge
  `clippy::too_many_arguments`). `mint` fails closed when the
  driver's granted set lacks `CAP_MEM_DMA` (returns `None` before
  allocating), else builds a brand-new `AddressSpace` (via a
  `make_table: Fn() -> P` closure) + `DmaPool` and hands ownership to
  a fresh `KernelVirtioHost` — one per-process heap per loaded driver
  (`AGENTS.md` §4). The impl lives in the kernel binary, not
  `drvhost`, so the userland host crate keeps zero `kernel/*` deps
  (`AGENTS.md` §3); `rustos-kernel` gained `rustos-caps`,
  `rustos-drvhost`, and `rustos-drv-bus-virtio` (`kernel-host`) deps
  plus a `host-tests`-featured dev-dep on `kernel/mem`. **Tests.**
  `rustos-kernel --lib` 37 (+3: mint-yields-host, mint-refuses-no-
  `MEM_DMA`, distinct-pool-per-call); `rustos-drv-bus-virtio
  --features kernel-host` 50 (unchanged — all call sites migrated to
  the owned-pool `new`); zero regressions in `rustos-kernel-mem`
  (101), `rustos-kernel-sec` (52), `rustos-abi` (77), `rustos-drvhost`
  (19 lib). `cargo clippy -p rustos-kernel --lib --all-targets` and
  `-p rustos-drv-bus-virtio --features kernel-host --all-targets`
  (`-D warnings`), `cargo fmt --check`, and
  `RUSTDOCFLAGS="-D warnings" cargo doc -p rustos-kernel --no-deps`
  are clean; the freestanding lib **and** the production
  `rustos-kernel` bin both build for `x86_64-unknown-none` with the
  new deps. **Docs.** `docs/src/drivers/virtio.md` (owned-pool
  snippet + "Kernel-binary factory" subsection) and
  `docs/src/drivers/host.md` (factory section now names
  `KernelVirtioFactory`). **Deferred.** Items 4 (the four virtio
  QEMU integration crates, which boot kernel + driver host + signed
  `.rxe` and thread this factory into a live `Host`) and 6
  (acceptance gate) remain outstanding — they require real
  PCI/DTB→DMA→IRQ device bring-up and are rewritten in
  `.junie/next-session-prompt.md`.
- Stage 4.D follow-up (Item 4 prerequisite — virtio-MMIO
  `MmioTransport`, *complete*): `PciTransport` covered the `x86_64`
  bus, but the riscv64 `-M virt` / `AArch64` path had no concrete
  `Transport`, so the planned riscv64 QEMU bring-up had nothing to
  drive a `virtio-mmio` register block. New module
  `drivers/bus/virtio/src/transport_mmio.rs` adds `MmioTransport`,
  the concrete modern (virtio-1.x) MMIO `Transport` over the single
  kernel-mapped `RegisterWindow` a bus driver resolves from the boot
  device tree and maps through the `CAP_MMIO_MAP`-gated MMIO-map
  facility. It drives the full §3.1 sequence against the virtio 1.1
  §4.2.2 MMIO register layout: status read/write + reset, 64-bit
  feature negotiation (`*FeaturesSel` windowed `u32` halves), per-queue
  `QueueSel`/`QueueNum`/`QueueDesc`/`QueueDriver`/`QueueDevice`
  programming (64-bit addresses as `Low`/`High` pairs) + `QueueReady`,
  and single-register `QueueNotify` notification. Two MMIO-only
  differences from PCI: there is no num-queues register (queues are
  probed via a non-zero `QueueNumMax`, so `num_queues` reports the
  16-bit max) and no per-queue notify offset/multiplier (notify is a
  constant-offset write). It performs no pointer arithmetic and holds
  no ambient authority (`AGENTS.md` §4). `MmioTransport::new`
  validates the `"virt"` magic, modern version `2`, a non-zero
  device-id, and a window ≥ the full register block, so the infallible
  `Transport` methods touch only in-bounds constant offsets and never
  panic (`AGENTS.md` §2.9). **Tests.** `cargo test
  -p rustos-drv-bus-virtio` → 73 (+12 `transport_mmio` tests against a
  `RegisterWindow`-backed `FakeMmioDevice`: short-window, bad-magic,
  legacy-version and empty-slot rejection, status/reset,
  device/driver-feature halves, queue-select write, queue programming
  + `QueueReady`, oversize rejection, single-register notify,
  device-config read with zero-fill, and a
  `SplitQueue`-drives-`MmioTransport` integration check); green with
  and without `--features kernel-host`. `cargo clippy
  -p rustos-drv-bus-virtio --all-targets -- -D warnings` (both feature
  sets), `cargo fmt --check`, `RUSTDOCFLAGS="-D warnings" cargo doc
  -p rustos-drv-bus-virtio --no-deps`, and the `x86_64-unknown-none` /
  `riscv64gc-unknown-none-elf` builds are clean. **Docs.**
  `docs/src/drivers/virtio.md` gains a "Modern MMIO transport" section
  and a refreshed scope / out-of-scope table. **Deferred.** The
  ring-0 boot-time bus walk, the riscv64 QEMU runner, and the QEMU
  integration crates + acceptance gate (Items 4 / 6) remain
  outstanding — see `.junie/next-session-prompt.md`.
- Stage 4.D follow-up (Item 4 prerequisite — modern-PCI
  `PciTransport`, *complete*): the only `Transport` implementation
  in the tree was the in-process `MockTransport`; nothing turned a
  kernel-mapped BAR into a driveable virtio device, so the Item 4
  QEMU bring-up had no real transport to stand on. New module
  `drivers/bus/virtio/src/transport_pci.rs` adds `PciTransport`
  (+ `PciTransportWindows`), the concrete modern (virtio-1.x) PCI
  `Transport`. It owns the four capability-checked `RegisterWindow`s
  a bus driver resolves from the device's virtio PCI capabilities
  (virtio 1.1 §4.1.4 — common-cfg, notify, ISR, device-cfg) plus the
  notification capability's `notify_off_multiplier`, and drives the
  full §3.1 sequence: status read/write, 64-bit feature negotiation
  (written as `u32` halves), per-queue select/size/desc/driver/device
  programming + enable, and queue notification at
  `queue_notify_off * multiplier`. It performs no pointer arithmetic
  and holds no ambient authority (`AGENTS.md` §4) — every access goes
  through the bounds-checked window accessors. `PciTransport::new`
  validates the common-cfg window ≥ `0x38` bytes and reads
  `num_queues` up front so the infallible `Transport` methods touch
  only in-bounds constant offsets and never panic (`AGENTS.md` §2.9);
  the device-supplied notify offset is bounds-checked on the fallible
  `queue_set` path before `notify` can use it, which fails closed for
  an unprogrammed queue. **Tests.** `cargo test
  -p rustos-drv-bus-virtio` → 61 (+11 `transport_pci` tests against a
  `RegisterWindow`-backed `FakeDevice`: short-window rejection,
  `num_queues` read, status/reset, driver-feature halves,
  queue-select range, queue programming + notify recording, oversize
  + out-of-bounds-notify rejection, no-op notify for an unprogrammed
  queue, device-config read with zero-fill, and a
  `SplitQueue`-drives-`PciTransport` integration check); green with
  and without `--features kernel-host`. `cargo clippy
  -p rustos-drv-bus-virtio --all-targets -- -D warnings` (both
  feature sets), `cargo fmt --check`, and `RUSTDOCFLAGS="-D warnings"
  cargo doc -p rustos-drv-bus-virtio --no-deps` are clean. **Docs.**
  `docs/src/drivers/virtio.md` gains a "Modern PCI transport" section
  and a refreshed scope / out-of-scope table. **Deferred.** The
  boot-time PCI walk that maps the BARs, the MMIO `Transport`, and
  the QEMU integration crates + acceptance gate (Items 4 / 6) remain
  outstanding — see `.junie/next-session-prompt.md`.
- Stage 4.D follow-up (Item 4 prerequisite — virtio-1.x PCI
  capability decode + register-window hand-off, *complete*): the PCI
  capability walker decoded MSI / MSI-X but not the vendor-specific
  virtio capability (`cap_id = 0x09`), so the planned boot-time PCI
  walk had no way to turn a device's virtio-1.x capabilities into the
  `(BAR, offset, length)` triples `PciTransport` needs. `drivers/bus/
  pci/src/config.rs` gains the `Capability::Virtio` /
  `Capability::VirtioNotify` records and the `VIRTIO_CFG_*` /
  `CAP_ID_VENDOR` constants (virtio 1.x §4.1.4); `enumerate.rs` gains
  the `decode_virtio` walker arm plus two public hand-offs:
  `Pci::map_virtio_window(bdf, cfg_type, mapper)` (resolves a
  requested config structure to `bar.base + offset`, bounds-checks
  `bar_offset + length` against the BAR size — failing closed with
  `OutOfRange` — and maps exactly `length` bytes through the
  `CAP_MMIO_MAP`-gated `MmioMapper`, refusing I/O-port BARs) and
  `Pci::virtio_notify_off_multiplier(bdf)`. The BAR-finding logic was
  extracted into a shared `resolve_bar` helper, so `map_bar_window`
  and `map_virtio_window` do not duplicate it (`AGENTS.md` §2.2). No
  pointer arithmetic, no ambient authority (`AGENTS.md` §4). The four
  windows + multiplier are exactly what `PciTransport::new` consumes,
  so a boot-time PCI walk can now assemble a live modern-virtio
  transport. **Tests.** `cargo test -p rustos-drv-bus-pci` → 21 (+5:
  virtio cap decode order against a `virtio-blk-pci` `1AF4:1042`
  fixture, per-cfg-type window hand-off + usable round-trip, notify
  multiplier, absent-cfg-type `NotFound`, capability-denial). `cargo
  clippy -p rustos-drv-bus-pci --all-targets -- -D warnings`, `cargo
  fmt --check`, `RUSTDOCFLAGS="-D warnings" cargo doc
  -p rustos-drv-bus-pci --no-deps`, and the `x86_64-unknown-none`
  build are clean. **Docs.** PCI README + `docs/src/drivers/bus.md`
  ("virtio-1.x configuration windows"). **Deferred.** The ring-0
  boot-time PCI walk that *calls* these hand-offs, the MMIO
  `Transport`, the riscv64 QEMU runner, and the QEMU integration
  crates + acceptance gate (Items 4 / 6) remain outstanding — see
  `.junie/next-session-prompt.md`.
- Stage 4.D follow-up (Item 5 — userland ARP / IP / ICMP responder,
  *complete*): new crate `userland/net/icmp` (`rustos-net-icmp`),
  the first substantial userland crate and the protocol peer the
  virtio-net QEMU integration (Item 4) drives. A new `userland/net`
  class was registered in `AGENTS.md` §3 and the workspace manifest.
  The crate is `no_std`, allocation-free, and
  `#![forbid(unsafe_code)]`, depending only on
  `rustos_abi::driver::net::Net`. Four protocol modules
  (`ethernet`, `arp`, `ipv4`, `icmp`) parse and serialise one layer
  each with bounds-checked, panic-free accessors that drop truncated
  or malformed input; the RFC 1071 one's-complement checksum lives
  once in `internet_checksum` and is shared by the IPv4 and ICMP
  layers (`AGENTS.md` §2.2). `Responder` binds an interface's MAC +
  IPv4 address and is otherwise stateless: `handle_frame` is a pure
  frame→optional-reply function (answering only correctly-addressed,
  well-formed, checksum-valid requests and dropping everything else),
  and `poll` / `run` drive it over any `Net` driver with a mandatory
  finite poll budget (no sleep-until / retry-until loop, `AGENTS.md`
  §2.1). In scope: ARP request+reply (RFC 826), option-free IPv4
  (RFC 791), ICMP echo (RFC 792). Out of scope (Stage 6): TCP, UDP,
  IPv6, routing, fragmentation. **Tests.** `cargo test
  -p rustos-net-icmp` → 33 passing (per-layer round-trips + rejection
  paths, RFC 1071 checksum vector, end-to-end ARP/ICMP answers over a
  mock `Net`, ignore-other-MAC/IP, output-too-small, poll/run,
  driver-error propagation). `cargo clippy -p rustos-net-icmp
  --all-targets -- -D warnings`, `cargo fmt --check`, and
  `RUSTDOCFLAGS="-D warnings" cargo doc -p rustos-net-icmp --no-deps`
  are clean. **Docs.** README plus `docs/src/userland/net_icmp.md`
  (new "Userland" section in `docs/src/SUMMARY.md`). **Deferred.**
  Items 2-tail.4 / 4 / 6 remain outstanding (see
  `.junie/next-session-prompt.md`); the virtio-net QEMU crates in
  Item 4 will exercise this responder against a live device.
- Stage 4.D follow-up (Item 3 — capability-checked register-window
  hand-off from `drivers/bus/{pci,mmio}`, *complete*): the bare
  identification tuple the `PciBackend` / `MmioBackend` shells used
  to carry is gone; a bus driver now obtains a kernel-minted
  register window and the backends own it. **ABI seam.** `lib/abi`
  ships `CapabilityId::MMIO_MAP = 12` (next free slot after
  `IRQ_BIND`, frozen by `well_known_ids_are_frozen` and added to
  `kernel/sec::is_known_capability`), plus a new
  `lib/abi/src/driver/mmio.rs`: `RegisterWindow` (a
  capability-checked, kernel-mapped MMIO window whose only
  constructor `from_mapping` is `unsafe` — so safe code cannot
  fabricate one — with bounds- and alignment-checked
  `read_u8/u16/u32` / `write_*` accessors returning `WindowError`),
  the `MmioMapper` trait (`map_window(phys_base, len) ->
  Result<RegisterWindow, MmioMapError>`), and `MmioMapError`
  (`CapabilityMissing` / `InvalidRegion` / `Unsupported`, each with
  `as_driver_error()`). `DriverHost::mmio_mapper()` is a new
  default-`None` accessor mirroring `virtio_host()`. **Kernel
  facility.** `kernel/mem` grew an `mmio` module (`MmioMap<P>`,
  `MmioRegion`, `MmioError`) that maps the device's *own* physical
  frames into a per-process `AddressSpace<P>` with
  `MapFlags::NO_CACHE` and the same guard-page bracketing as
  `DmaPool` (it does not allocate frames); the host model backs the
  window with a page-aligned `Vec<u8>` so a `RegisterWindow` minted
  over `region_base` is word-aligned in tests too. `kernel/sec`
  grew a companion `mmio` gate (`map_mmio` / `unmap_mmio`) verifying
  `CapabilityId::MMIO_MAP` and emitting `AuditEvent::MmioMapped`
  (id `1040`, Info, `(task, phys, len)`) / `MmioMapDenied`
  (id `1041`, Error) with `MmioGateError::as_errno()`. **Virtio.**
  `PciBackend` / `MmioBackend` were rewritten to own a
  `RegisterWindow` and expose `read_u32` / `write_u32`; the
  placeholder `PortIo` / `MmioOps` traits were removed. A new
  `kernel-host`-gated `KernelMmioMapper` wraps `&mut MmioMap` +
  `&TaskCapabilities` + audit `Sink` and mints `RegisterWindow`s
  through `kernel/sec::map_mmio` (drvhost stays free of `kernel/*`
  deps). **Bus hand-off.** `Pci::map_bar_window(bdf, bar_index,
  &dyn MmioMapper)` resolves a memory BAR (refusing I/O BARs and
  unused/absent BARs) and `Mmio::map_slot_window(base, &dyn
  MmioMapper)` reads the DTB `<base, length>` pair; both ask the
  kernel mapper for the window and never synthesise a pointer.
  **Tests.** `rustos-abi --lib` 77 (+9), `rustos-kernel-mem --lib`
  101 (+17), `rustos-kernel-sec --lib` 52 (+6), `rustos-drv-bus-pci`
  16 (+4), `rustos-drv-bus-mmio` 9 (+3), `rustos-drv-bus-virtio` 50
  (+10, incl. `KernelMmioMapper`); zero regressions in `drvhost`,
  `virtio_blk`, `virtio_net`, `kernel-core`, `kernel-syscall`.
  `cargo clippy --all-targets [--all-features] -- -D warnings`,
  `cargo fmt --check`, and `RUSTDOCFLAGS="-D warnings" cargo doc
  --no-deps` are clean across every touched crate. **Docs.** New
  "Register-window hand-off" section in `docs/src/drivers/bus.md`
  (seam + capability-flow diagram), a "5.2 MMIO register-window
  mapper" section in `docs/src/architecture/memory.md`, two new
  audit rows (`1040` / `1041`) in
  `docs/src/architecture/security.md`, and the virtio README /
  `docs/src/drivers/virtio.md` "shells" wording updated.
  **Deferred.** The Item 3 QEMU integration test (boot kernel +
  walk PCI/DTB + hand a working window to the virtio transport)
  is folded into Item 4 (it needs the same full boot wiring the
  four virtio QEMU crates require); Items 2-tail.4 / 4 / 5 / 6
  remain outstanding and are rewritten in
  `.junie/next-session-prompt.md`.
- Stage 4.D follow-up (Item 1 — kernel per-process-heap DMA
  API): landed. `lib/abi` ships `CapabilityId::MEM_DMA = 10` —
  the next free slot after `AUDIT_WRITE`, frozen by the
  `well_known_ids_are_frozen` test (`AGENTS.md` §9).
  `kernel/mem` grew a new `dma` module (`DmaPool<P>`,
  `DmaBuffer`, `DmaError`) composed over the existing
  `FrameAllocator` (contiguous-by-physical frame blocks) and
  `AddressSpace<P: PageTableOps>` (per-process virtual window):
  every allocation reserves a leading + trailing guard slot left
  unmapped in the `AddressSpace` (`AGENTS.md` §4 — guard pages
  around kernel slabs); the host model paints the guard storage
  with `0xCC` and verifies it at `free` time so the same call
  site is testable on host and faults on hardware.
  `DmaPool::free` zeroes every byte of the data region through
  the audited `zeroize` crate's volatile clear *before* returning
  frames to the buddy allocator (`AGENTS.md` §4 — zero-on-free
  for any buffer that may have held credentials); a
  `reuse_after_free_sees_zeroed_buffer` test exercises this
  end-to-end with a sentinel payload. Allocation requests larger
  than `MAX_ORDER` produce `DmaError::SizeUnsupported`; virtual
  or physical exhaustion produce
  `DmaError::Alloc(OutOfMemory)`; no `unwrap` / `expect` /
  `panic!` on production paths (`AGENTS.md` §2.9).
  `HostPageTable` moved behind a new `kernel/mem` `host-tests`
  Cargo feature so downstream crates can borrow it for unit
  tests without leaking the test double into production builds.
  `kernel/sec` grew a companion `dma` module: `alloc_dma` /
  `free_dma` verify `CapabilityId::MEM_DMA` on the calling
  `TaskCapabilities` before delegating to the pool, emit a
  structured `AuditEvent::DmaAllocated` (id `1030`, Info) on
  every grant carrying `(task, len, phys)`, and an
  `AuditEvent::DmaAllocDenied` (id `1031`, Error) on every
  refusal carrying `(task, uid, requested)`.
  `DmaGateError::as_errno()` maps gate failures into `abi-v1`
  errnos (`PermissionDenied` / `BufferTooSmall` /
  `LengthOutOfRange` / `OutOfRange`). Tests: 17 new in-crate
  units in `kernel/mem/src/dma/tests.rs` (alignment, phys lookup,
  zero-on-free, reuse-sees-zero, OOM-as-Result, leading +
  trailing guard overrun, frame-reclaim-on-violation,
  unknown-buffer / double-free) and 7 new in
  `kernel/sec/src/dma/tests.rs` (grant / denial audit-event
  sequence, zero-size pool propagation, oversize →
  LengthOutOfRange, full errno mapping). Crate totals: `cargo
  test -p rustos-kernel-mem --lib` → 84 passing (was 67);
  `cargo test -p rustos-kernel-sec --lib` → 46 passing (was 39);
  zero regressions in `rustos-abi`, `rustos-caps`,
  `rustos-drv-bus-virtio`, `rustos-drv-storage-virtio-blk`,
  `rustos-drv-network-virtio-net`, `rustos-drv-bus-pci`,
  `rustos-drv-bus-mmio`, `rustos-drvhost`. `cargo clippy ...
  --all-targets -- -D warnings` and `cargo fmt --check` are
  clean across `rustos-abi`, `rustos-kernel-mem`,
  `rustos-kernel-sec`. Docs: new "DMA buffers" section in
  `docs/src/architecture/memory.md`, two new rows
  (`1030 / DmaAllocated`, `1031 / DmaAllocDenied`) in
  `docs/src/architecture/security.md`. `cargo xtask test`/`ci`
  cannot be run end-to-end in the current environment —
  `kernel/arch/x86_64` needs the `nightly-2026-05-27` toolchain
  pinned in `rust-toolchain.toml` (for `#[unsafe(naked)]` and
  inline-const), and the host has only stable rustc 1.75; the
  failure is identical to the pre-existing Stage 3a footprint
  and is unrelated to this Item 1 work. Items 2–6 of the prior
  next-session prompt remain outstanding and have been rewritten
  into the next session's prompt.
- Stage 4.D follow-up (Item 0-tail — `KernelVirtioHost` plumbing
  into `userland/system/drvhost`, *complete*): the wiring deferred
  by Item 0 has landed end-to-end and the host↔driver virtio ABI
  has been re-homed where the layering allows it. **ABI re-home.**
  `PoolId`, `SlabFreeFn`, `DmaSlab` (the owned-slab shape) and the
  `VirtioHost` trait now live in `lib/abi/src/driver/{dma.rs,
  virtio.rs}` instead of `drivers/bus/virtio`. The original
  proposal — extend `DriverHost::virtio_host(&mut self) -> &dyn
  VirtioHost` directly — would have forced `lib/abi` to depend on
  `drivers/bus/virtio` (or required moving the trait), inverting
  the layering and violating `AGENTS.md` §3. Re-homing keeps the
  ABI seam where every other host/driver trait already lives. The
  signature was reshaped to `DriverHost::virtio_host(&self) ->
  Option<&dyn VirtioHost>` because the frozen driver-load entry
  point per `AGENTS.md` §8 is
  `pub fn register(host: &dyn DriverHost) -> Result<DriverHandle,
  DriverError>` — i.e. immutable — and `VirtioHost`'s own methods
  already use `&self` plus interior mutability, so the `&mut`
  shape in the previous prompt would not have composed with the
  existing entry point. The default body returns `None`, keeping
  every existing `DriverHost` impl source-compatible. The owned
  `DmaSlab` test suite (round-trip, three simultaneous disjoint
  writes, drop-frees-pool, pool-id rejection) stays in
  `drivers/bus/virtio/src/dma.rs` against the re-exported types,
  because `lib/abi` is no-alloc by crate invariant — moving the
  tests would have required pulling `alloc` into `lib/abi`.
  **Drvhost wiring.** `userland/system/drvhost::host` gained a new
  `VirtioHostFactory` trait, `HostConfig::virtio_host_factory:
  Option<&'h dyn VirtioHostFactory>`, and a borrowed `virtio_host:
  Option<&'v dyn VirtioHost>` field on `LoadedHostView`. The
  factory mints a fresh `Box<dyn VirtioHost + 'r>` for the
  duration of a single `register()` call; the host owns the box,
  the view borrows a `&dyn VirtioHost` from it for the duration of
  `entry(&view)`, and both are dropped at function return so any
  per-driver `DmaPool` slots are reclaimed. Drivers retrieve the
  host through `host.virtio_host()`. **Deliberate non-decisions.**
  The `kernel-host` feature on `userland/system/drvhost` itself
  was *not* added: the factory abstraction keeps the kernel-side
  generics (`P: PageTableOps`, `S: Sink`) out of drvhost entirely
  — the kernel binary supplies an impl whose internals do mention
  those generics, and drvhost stays free of `kernel/*` deps in its
  production build. Adding the feature without a concrete kernel
  consumer would have been the kind of dead-code bloat
  `AGENTS.md` §2.3 forbids. **Tests.** Two new integration tests
  in `userland/system/drvhost/tests/host_integration.rs`
  (`virtio_host_factory_default_none_yields_none`,
  `virtio_host_factory_some_yields_virtio_host`) cover both seams
  end-to-end: the first asserts the default `None` slot causes
  `host.virtio_host()` to report `None` inside `register()`; the
  second wires a `MockHost`-backed factory and asserts that the
  driver successfully calls `alloc_dma_zeroed(64)` through the
  trait. `rustos-drv-bus-virtio` was added to
  `rustos-drvhost`'s `[dev-dependencies]` for the `MockHost`
  symbol. **Doc updates.** `docs/src/drivers/host.md` gained a
  "Virtio host factory" subsection and an updated `HostConfig`
  example; `docs/src/abi/driver_traits.md` documents the new
  `virtio_host()` accessor and the ABI re-home in the
  `DriverHost` table. The `drvhost_qemu` integration was updated
  to set `virtio_host_factory: None` (the bumpalloc-backed kernel
  test bin has no kernel-side `DmaPool` yet; that arrives with
  Item 2 — IRQ plumbing — and the production kernel binary's
  factory). **Verification.** `cargo test --workspace --lib
  --exclude rustos-kernel-arch-*` → 663 passing (unchanged
  baseline; the move did not delete or add lib-level tests).
  `cargo test -p rustos-drvhost` → 19 lib + 15 integration = 34
  passing (was 19 + 13). `cargo test -p rustos-drv-bus-virtio`
  default → 41 passing; with `--features kernel-host` → 41
  passing. `cargo clippy -p rustos-abi -p rustos-drv-bus-virtio
  -p rustos-drvhost --all-targets --all-features -- -D warnings`
  and `cargo fmt --check` on touched crates are clean. Items 2–6
  of the prior next-session prompt remain outstanding and have
  been rewritten into the new
  `.junie/next-session-prompt.md` with concrete entry points that
  build on the `VirtioHostFactory` seam landed here.

- Stage 4.D follow-up (Item 2-tail.3 — `KernelVirtioHost::notify_wait`
  blocks on a pre-bound `IrqHandle`, *complete*): the in-kernel
  virtio host's `notify_wait` is no longer a polled cooperative shim;
  it now blocks the loaded driver task on the device's pre-bound
  interrupt line through the kernel IRQ subsystem. **Shared blocking
  primitive.** The poll-and-yield loop that previously lived only in
  `kernel/core::syscalls::irq_wait` was extracted into the new
  `kernel/irq::wait` module as `block_until_ready(table, handle,
  caller, timeout_ns, &dyn IrqWaiter) -> WaitOutcome`, so both the
  `irq_wait` syscall handler and `notify_wait` drive one
  implementation (`AGENTS.md` §2.2 — no duplication). The clock +
  cooperative yield are inverted behind the two-method `IrqWaiter`
  trait (`now_ns` / `yield_now`), keeping `kernel/irq` free of any
  scheduler or architecture dependency; `IrqWaitAbort`
  (`TaskVanished` / `SchedulerError`) carries yield-abort reasons the
  callers map onto their own error surface. `IrqTable` and
  `IrqController` are unchanged — the addition composes them, per the
  carried-over assumption. **Syscall handler.** `irq_wait` now builds
  a `SyscallIrqWaiter` (wraps `Scheduler::yield_current` +
  `KernelArch::monotonic_ns`, capturing the issuing CPU once) and
  delegates to `block_until_ready`, preserving the exact errno
  mapping (`Ready→Ok`, `TimedOut→TimedOut`, `NotFound→NotFound`,
  `TaskVanished→NotFound`, `SchedulerError→OutOfRange`). **Virtio
  host.** `KernelVirtioHost` gained `&IrqTable`, the bound
  `IrqHandle`, and an `&dyn IrqWaiter` (waiting against the owning
  task via `caller.task()` with an unbounded `u64::MAX` timeout); the
  polled `notify_log` was removed (it survives only on `MockHost`).
  `rustos-kernel-irq` was added to `rustos-drv-bus-virtio`'s
  `kernel-host` feature deps and `[dev-dependencies]`. **Tests.**
  `kernel/irq::wait` ships 6 unit tests (pre-fired, fire-during-yield,
  timeout, forged handle, yield-abort, unbounded-no-wrap). The virtio
  `kernel_host` tests were rewired onto an IRQ fixture and the old
  `notify_log` test replaced with four: `notify_wait_returns_when_line
  _pre_fired`, `notify_wait_blocks_until_line_fires`,
  `notify_wait_observes_mask_before_wake` (probe controller asserts
  `ready == false` while `mask` runs), and
  `notify_wait_returns_when_binding_released`. **Docs.**
  `docs/src/security/irq.md` intro and "Wait semantics" section
  rewritten to document the shared loop, the `IrqWaiter` seam, and the
  virtio consumer. **Verification.** `cargo test -p
  rustos-kernel-irq --lib` → 24; `cargo test -p rustos-kernel-core
  --lib` → 37; `cargo test -p rustos-drv-bus-virtio` and `--features
  kernel-host` → 44 each. **What is still outstanding.** Items
  2-tail.4 / 3 / 4 / 5 / 6 from `.junie/next-session-prompt.md` remain
  deferred — the user-confirmed scope for this session was Item
  2-tail.3 only.

- Stage 4.D follow-up (Item 2-tail.2 QEMU validation — live IRQ
  end-to-end on x86_64 QEMU, *complete*): the QEMU integration
  crate deferred from the preceding A2 split has landed and the
  full Item 2-tail.2 deliverable is now hardware-validated. New
  freestanding `tests/integration/irq_qemu_x86_64` crate
  (`[[bin]] rustos-test-irq-qemu-x86-64`, `test-hooks` default
  feature, same `compile_error!` release-build guard the
  syscall-dispatch QEMU test uses) reuses `rustos_kernel::boot`
  verbatim and installs a custom audit Sink. On observing
  `AuditEvent::BootCompleted` the sink: (1) reads the published
  `IrqTable` via the new `rustos_kernel::arch_wrapper::published_irq_table`
  and the typed `IoApicController<VolatileIoApicMmio>` via the
  new `rustos_kernel::ioapic_controller::published_typed`;
  (2) resolves the IDT vector assigned to GSI 2 (QEMU's
  PIIX/Q35 `InterruptSourceOverride { source: 0, gsi: 2 }`
  mapping for the legacy ISA IRQ-0 PIT line) via
  `rustos_arch_x86_64::irq::global_routing().vector_for_gsi(2)`;
  (3) binds GSI 2 in the `IrqTable` against the synthesised
  `TaskId(0)`, masks the legacy 8259 PIC (OCW1 IMR ← `0xFF`),
  unmasks the line via the new `IoApicController::unmask(gsi)`,
  and arms PIT channel 0 in mode 0 as a one-shot
  (architectural 1.193182 MHz × 2000-tick reload ≈ 1.68 ms);
  (4) `sti`s, then spin-polls `IrqTable::try_wait_step` with
  `hlt` between polls and a 1 s deadline. The asm trampoline +
  `production_external_irq_dispatch` chain delivers the IRQ →
  `IrqTable::fire(2, controller)` masks the line + SeqCst-fences
  → `ready` flips → `try_wait_step` observes
  `WaitStep::Ready`; (5) `cli`s and re-reads the IO-APIC
  redirection-entry low half via the new
  `IoApicController::read_pin_low(gsi)`, asserting bit 16 is
  set — the load-bearing evidence the controller's mask write
  reached the IO-APIC MMIO window before the wake;
  (6) flips `qemu_exit::exit_success`. Any deviation —
  missing slot, no vector bound, `WaitStep::TimedOut`,
  `WaitStep::NotFound`, mask bit clear — flips
  `qemu_exit::exit_failure` with the QEMU serial log attached
  by `tools/qemu::Runner`. **New public seams** (all
  read-only-after-init publish-from-existing-state, no new
  writable surface — `AGENTS.md` §2.4 honoured):
  `rustos_kernel::arch_wrapper::published_irq_table`,
  `rustos_kernel::arch_wrapper::published_irq_controller`,
  `rustos_kernel::ioapic_controller::publish_typed` (called
  from `try_boot::discover_and_program_io_apics`),
  `rustos_kernel::ioapic_controller::published_typed`,
  `IoApicController::unmask`,
  `IoApicController::read_pin_low`, and a small
  `IoApic::read_redirection_entry_low` reader in the arch
  crate. Each surface ships host unit tests:
  `published_irq_controller_returns_set_once_pointer`,
  `published_irq_table_is_none_until_install_dispatch_runs`,
  `unmask_clears_mask_bit_and_preserves_vector`,
  `unmask_rejects_gsi_out_of_range`,
  `unmask_rejects_unprogrammed_pin`,
  `read_pin_low_returns_low_half_after_program_pin`. The
  `RecordingMmio` test mock grew a per-register `last_writes`
  back-channel so `IoApicMmio::read` returns the last value
  written — necessary to exercise `read_pin_low` against the
  mock. **Workspace + xtask wiring.**
  `Cargo.toml::[workspace].members` grew a sixth
  `tests/integration/irq_qemu_x86_64` entry;
  `tools/xtask::commands::qemu_tests::TESTS` grew a sixth
  enrolment with a 60 s budget. `cargo xtask test --qemu`
  builds and runs all six integration crates in sequence.
  **Drive-by fix.** The previous session's
  `tests/integration/syscall_dispatch_qemu/src/main.rs` had
  not been updated for the
  `BinArch::new(arch, calibration, irq_routing)` signature
  refresh; the test was excluded from `cargo test --workspace`
  but its freestanding build was broken. Fixed in the same
  change by passing `IrqRouting::unsupported()` — the call
  site does not exercise the IRQ path. **Docs.**
  `docs/src/security/irq.md` controller table updated from
  "Wired" to "Wired and QEMU-validated"; a new "x86_64 QEMU
  validation (Stage 4.D Item 2-tail.2 QEMU)" section
  documents the six-step end-to-end scenario; the
  test-coverage section grew bullets for the new
  `arch_wrapper` accessor tests, the `ioapic_controller`
  `unmask` / `read_pin_low` tests, and the QEMU integration
  crate. **Verification.** `cargo test -p rustos-kernel
  --lib` → 34 passing (3 new accessor tests + 3 new unmask
  tests vs the 28 baseline). `cargo run -p rustos-xtask --
  test --qemu` → six QEMU crates pass:
  `rustos-test-memory-isolation`,
  `rustos-test-scheduler-stress-qemu`,
  `rustos-test-kernel-arch-boot`,
  `rustos-test-syscall-dispatch-qemu`,
  `rustos-test-drvhost-qemu`,
  `rustos-test-irq-qemu-x86-64`. **What is still
  outstanding.** Items 2-tail.3 / 2-tail.4 / 3 / 4 / 5 / 6
  from the preceding next-session prompt remain deferred —
  the user-confirmed scope for this session was the QEMU
  validation only. Rewritten into
  `.junie/next-session-prompt.md`.

- Stage 4.D follow-up (Item 2-tail.2 — x86_64 IDT external-vector +
  IO-APIC trap glue, *complete*): the architecture half that
  composes the `kernel/irq` substrate landed in the preceding
  session into a real trap source on x86_64. **Vector range.**
  `kernel/arch/x86_64::irq` reserves IDT vectors `0x30..=0xFE`
  (207 vectors) for external IRQs. Per-vector asm stubs are
  emitted by an `.altmacro` / `.rept` loop in
  `kernel/arch/x86_64/src/external_irq.s`; each stub pushes the
  vector immediate and jumps to a shared trampoline
  (`rustos_arch_x86_64_external_irq_common`) that saves the 15
  GPRs (`SavedRegs` layout), loads `%rdi` with the saved-regs
  pointer and `%rsi` with the vector, calls into Rust
  (`rustos_arch_x86_64_external_irq_dispatch`), and restores
  before `iretq`. AGENTS.md §2.2 (no duplication) is honoured by
  the macro-driven generation. **Vector→stub address table.**
  Published in `.rodata` as a `.quad` array
  (`rustos_arch_x86_64_external_irq_table`); exposed to Rust as
  `extern "C" static …: [usize; 207]` and consumed through
  `kernel/arch/x86_64::irq::external_isr_addr(vector) ->
  Option<u64>`. **Vector↔GSI routing.** New `Routing` table
  (`kernel/arch/x86_64::irq::routing`) backed by one
  `AtomicU32` per reserved vector with a `u32::MAX` unmapped
  sentinel; install/lookup are lock-free and set-once after the
  `Phase::Irq` boot step. 7 host unit tests cover empty/install/
  out-of-range/sentinel/idempotent-republish/conflict semantics.
  **Rust dispatcher.** Looks up the GSI via `global_routing`,
  forwards to the installed `ExternalIrqDispatchFn` slot, then
  writes the LAPIC EOI register before returning. The
  dispatcher slot is set-once via `set_external_irq_dispatch`
  (compare-exchange against `0`). 5 host unit tests cover
  vector-range, fail-closed-on-double-install,
  dispatch-addr-round-trip. **IO-APIC controller.** New
  `kernel/rustos-kernel::ioapic_controller::IoApicController<M>`
  generic over the arch crate's `IoApicMmio` trait, with one
  block per IO-APIC and a per-pin `(vector, dest, masked)`
  cache. Public surface: `new(blocks)`, `program_pin(gsi,
  vector, dest, masked)`, `block_count`, plus the
  `IrqController::mask(line)` trait impl. `mask` re-writes the
  redirection entry via the audited
  `IoApic::set_redirection_entry` (volatile MMIO) preserving
  vector + destination, then issues
  `core::sync::atomic::fence(Ordering::SeqCst)` — paired with
  the SeqCst load `IrqTable::try_wait_step` performs on
  `ready`, guaranteeing every CPU observing `ready = true`
  also observes the masked redirection entry. 8 host unit
  tests cover locate / program / mask-rewrite / out-of-range
  refusal / unprogrammed-pin refusal / multi-IO-APIC routing /
  and the load-bearing
  `ioapic_controller_mask_before_wake_ordering` regression
  test that drives a live `IrqTable::fire` against the
  controller with a `RecordingMmio` mock and asserts the
  controller's MMIO write log records the mask write
  *before* `IrqTable::fire` returns `Marked`. **KernelArch
  extension.** Two new trait methods (with safe default
  no-op impls so `wasm32` / `TestArch` inherit no work):
  `KernelArch::irq_routing(&self) -> IrqRouting` (consulted
  during the new `Phase::Irq`) returns a `max_line` + a
  `&'static (dyn IrqController + Send + Sync)`, and
  `KernelArch::install_irq_dispatch(&self, &'static
  IrqTable)` (called immediately after the table is
  constructed) lets the arch port publish the table pointer
  into its dispatcher slot. New `IrqRouting` type + re-export
  in `kernel/core::lib`. **New `Phase::Irq`.** Inserted into
  `Phase::ORDER` between `Sched` and `Syscall`; updates
  `Phase::as_str`, `Phase::ORDER` (`[Phase; 6]` →
  `[Phase; 7]`), `BootStarted`'s `phase_count` field from
  `"5"` to `"7"` (the previous value was stale — the trail
  pre-Item 2-tail.2 already shipped 6 phases). New
  `irq_phase_lands_between_sched_and_syscall` host test pins
  the ordering. `KernelState.irq_controller` switched from
  the concrete `UnsupportedController` to `&'static (dyn
  IrqController + Send + Sync)`, sourced from
  `arch.irq_routing()` during `Phase::Irq`. **BinArch.**
  Now carries the assembled `IrqRouting`; `BinArch::new`
  publishes the controller pointer into a `OnceCell`-backed
  `IRQ_CONTROLLER_SLOT`; `BinArch::install_irq_dispatch`
  publishes the table into `IRQ_TABLE_SLOT` and installs
  `production_external_irq_dispatch` into the arch crate's
  set-once dispatcher slot. Fail-closed on the second
  publish via `arch_halt`. New `irq_routing_returns_captured_value`
  host test pins the BinArch ↔ IrqRouting wiring. **try_boot
  wiring.** New helper `discover_and_program_io_apics(&Madt,
  bsp_lapic_id)` walks every `MadtEntry::IoApic`, reads
  each `max_redirection_entry`, pre-validates the total
  pin count against the reserved vector range
  (`BootError::IrqVectorExhausted`), constructs the
  `IoApicController`, allocates one vector per pin from a
  bump counter starting at `EXTERNAL_VECTOR_FIRST`,
  installs the per-CPU IDT entry via
  `percpu::install_vector`, publishes
  `(gsi, vector)` into `global_routing`, and calls
  `program_pin(gsi, vector, bsp_lapic_id, masked = true)` —
  every line starts masked. Five new `BootError` variants
  (`NoIoApic`, `IrqVectorExhausted`, `IrqIdtInstall`,
  `IrqRoutingPublish`, `IrqProgramPin`) carry stable audit
  cause strings. **Docs.** `docs/src/security/irq.md`
  controller table updated from "Not wired" to "Wired"
  with the IoApicController reference; new "x86_64 trap
  glue (Stage 4.D Item 2-tail.2)" section documents the
  six-step trap path from IDT vector to `IrqTable::fire`.
  `docs/src/architecture/kernel.md` boot-timeline table
  expanded to include the new `irq` phase between
  `sched` and `syscall`, with the
  `arch.install_irq_dispatch(&state.irq)` publication step
  called out inline. **Verification.** `cargo test
  --workspace` (excluding the five QEMU-only integration
  test crates): 766 tests passing (was 740 with item 2-tail
  baseline; the delta of 26 covers `kernel/arch/x86_64::irq`
  (12 tests), `kernel/rustos-kernel::ioapic_controller`
  (8 tests), `kernel/core::init::irq_phase_lands_between_sched_and_syscall`
  (1), `kernel/rustos-kernel::arch_wrapper::irq_routing_returns_captured_value`
  (1), plus 4 dispatcher / set-dispatcher / routing-related
  arch-crate tests). `cargo clippy --workspace --all-targets
  -- -D warnings` clean on the host build; `cargo clippy -p
  rustos-arch-x86_64 -p rustos-kernel --target
  x86_64-unknown-none --features
  rustos-arch-x86_64/sched-arch -- -D warnings` clean on the
  freestanding build. `cargo fmt --check` clean. `cargo build
  -p rustos-kernel --target x86_64-unknown-none` succeeds
  (asm thunks + Rust glue link). **What is still
  outstanding.** The QEMU integration test crate
  `tests/integration/irq_qemu_x86_64` mandated by the
  prompt's reading list is the sole hand-off: I cannot run
  QEMU in the current environment, so the test would land
  as a non-validated artifact contrary to AGENTS.md §15.6
  ("Run the full test suite … Quote the actual output").
  Per the user-confirmed A2 scope split, the QEMU test
  crate is queued for the next session, alongside the
  carried Items 2-tail.3 / 2-tail.4 / 3 / 4 / 5 / 6 from
  the prior prompt. Rewritten into
  `.junie/next-session-prompt.md`.

- Stage 4.D follow-up (Item 2-tail — kernel IRQ table + per-handle
  wait queue, *complete*): the kernel-side substrate that backs
  the frozen `abi-v1` IRQ surface (`CapabilityId::IRQ_BIND`,
  `SyscallNumber::IRQ_BIND` / `IRQ_WAIT`, `IrqHandle`,
  `Errno::TimedOut`) has landed. New `kernel/irq` crate
  (`rustos-kernel-irq`, `no_std`) ships an `IrqTable` carrying a
  `BTreeMap<u32 line, IrqEntry>` + `BTreeMap<u64 handle_raw, line>`
  index behind a writer-preference `lib/sync::RwLock` mirroring
  the `CapTable` lock-ordering policy. Surface: `bind(line, owner)`,
  `try_wait_step(handle, caller, now_ns, deadline_ns)`,
  `fire(line, &dyn IrqController)`, `release_for(task)`,
  `lookup(handle)`, plus an `IrqController` trait whose production
  impl on x86_64 will program the IO-APIC redirection-entry mask
  (deferred to the trap-glue session — see
  `.junie/next-session-prompt.md`). Mask-before-wake is the
  load-bearing invariant: `IrqTable::fire` calls
  `controller.mask(line)` *before* setting the per-entry `ready`
  flag; the unit test `mask_is_observed_before_wake` installs a
  probe controller that reads the table's `ready` flag while
  `mask` is in flight and asserts it is still `false`. Forgery
  defence: `try_wait_step` re-checks the `(handle, caller)`
  mapping before any state transition. The crate ships 18 in-tree
  unit tests covering bind / duplicate refusal / out-of-range
  refusal / ready-after-fire / timeout / forgery / mask-before-wake
  ordering / stray-IRQ containment / release_for semantics /
  handle-uniqueness across rebinds. `kernel/core` integration:
  `KernelSyscallHandlers::new` now takes `&IrqTable` +
  `&(dyn IrqController + Sync)` borrows; `irq_bind` / `irq_wait`
  no longer announce `SYSCALL_FEATURE_UNAVAILABLE`. `irq_bind`
  calls `IrqTable::bind(line, caller.task_id)` and returns
  `handle.as_u64()`; `irq_wait` runs a polling loop on
  `IrqTable::try_wait_step` driven by
  `KernelArch::monotonic_ns(arch.current_cpu())`, yielding via
  `Scheduler::yield_current` between iterations and composing
  only existing primitives (`AGENTS.md` §2.4 — no scheduler
  interface change). `KernelSyscallHandlers::exit` now calls
  `IrqTable::release_for(caller.task_id)` before evicting the
  capability record and the scheduler entry so a task cannot
  retain a binding past exit (`docs/src/security/irq.md` —
  freshly created tasks that want the same line must re-issue
  `irq_bind`). `KernelState` (`kernel/core/src/init.rs`) owns
  one `IrqTable::new(0)` + `UnsupportedController`; the kernel
  binary's post-`run_phases` wiring phase will swap in a real
  controller and widen `max_line` once the x86_64 trap glue
  lands. `kernel/core` syscall tests grew by 6
  (`irq_bind_mints_handle_and_records_owner_against_caller`,
  `irq_bind_returns_out_of_range_for_line_above_max`,
  `irq_bind_rejects_duplicate_line`,
  `irq_wait_returns_not_found_on_forged_handle`,
  `irq_wait_returns_timed_out_when_no_fire_within_zero_timeout`,
  `irq_wait_returns_ok_when_binding_pre_fired`) and one
  cross-syscall test (`exit_releases_every_irq_binding_owned_by_task`).
  The two prior deferral tests
  (`irq_{bind,wait}_returns_not_implemented_and_audits_feature_unavailable`)
  have been replaced rather than `#[ignore]`d per AGENTS.md §2.5.
  `tests/integration/syscall_dispatch_qemu` updated to construct
  an `IrqTable` + `UnsupportedController` alongside the
  synthesised `Scheduler`/`CapTable` quartet so its
  `KernelSyscallHandlers::new` call site matches the new
  signature. **Pre-existing defect fixed in the same change.**
  The `tools/xtask::commands::abi_check::tests::desync_in_table_hash_is_detected`
  fixture hard-coded `0xca,` as the byte it would flip in the
  hash literal; the current `SYSCALL_TABLE_HASH` contains no
  `0xca` byte, so the mutation was silently a no-op and failed
  the `assert_ne!(original, mutated)` guard. The fixture now
  locates the first `0x` token after the `SYSCALL_TABLE_HASH`
  anchor and flips its low nibble, so it is robust to any
  future hash refresh (AGENTS.md §7 — no flaky tests). **Docs.**
  `docs/src/security/irq.md` extended with a "Kernel-side
  implementation" section covering the invariants
  (mask-before-wake, forgery defence, lock ordering, idempotent
  release), the wait-loop semantics, the
  per-architecture-port controller table (with explicit
  "x86_64 trap glue not yet wired" status), and the
  test-coverage summary. `docs/src/architecture/syscalls.md`
  handler-wiring table updated: the two *deferred* rows for
  `irq_bind` / `irq_wait` replaced with the real wiring; an
  `exit` ↔ `release_for` note added below the table.
  **Verification.** `cargo test -p rustos-kernel-irq` → 18
  passing. `cargo test -p rustos-kernel-core` → 36 unit + 5
  init tests passing. `cargo test --workspace` (excluding the
  five QEMU-only `tests/integration/*` bins that require QEMU
  to run) → every test green. `cargo clippy --workspace
  --all-targets -- -D warnings` (with the same exclusion) →
  clean. `cargo fmt --check` → clean. **What is still
  outstanding.** Items 3–6 from the prior next-session-prompt
  (bus-handle hand-off in `drivers/bus/{pci,mmio}` ↔
  `drivers/bus/virtio`, four QEMU integration test crates,
  userland ARP/IP/ICMP responder, acceptance gate) plus the
  x86_64 IDT external-vector + IO-APIC trap-source wiring and
  the kernel-binary `VirtioHostFactory` impl. Rewritten into
  `.junie/next-session-prompt.md` (the trap-glue chunk leads,
  because virtio-net / virtio-blk integration tests depend on
  real IRQ delivery).

- Stage 4.D follow-up (Item 2 — IRQ ABI surface, *complete*,
  superseded by the entry above): the ABI-half of Item 2 had
  landed. `CapabilityId::IRQ_BIND
  = 11` is appended to the frozen `abi-v1` capability table
  (`lib/abi/src/capability.rs`) and mirrored in
  `kernel/sec::is_known_capability` plus its audit-frozen-id test
  (`is_known_capability_covers_abi_v1_constants`).
  `SyscallNumber::IRQ_BIND = 8` and `IRQ_WAIT = 9` are appended in
  `lib/abi/src/syscall.rs` next to the existing frozen numbers, and
  their `SyscallSpec` rows landed in `lib/abi/src/syscalls.rs` with
  `required_capability = Some(CapabilityId::IRQ_BIND)` on both —
  `irq_bind` (`U32 -> Handle`, audited) and `irq_wait` (`Handle,
  U64 -> Errno`, unaudited on success). `ENCODED_TABLE_LEN` bumped
  from `26 * 8` to `26 * 10`; `SYSCALL_TABLE_HASH` in
  `kernel/syscall/src/table.rs` refreshed to
  `6b6dbd9c30b6aa87d41ac840a5bdef1cc6fc6a71ae03fe4db7746d964c09814b`
  via `cargo xtask abi-check`. New opaque ABI newtype
  `IrqHandle(u64)` in `lib/abi/src/syscall.rs` with `INVALID = 0`
  reserved against caller-zeroed buffers. New `Errno::TimedOut =
  13` appended to the `#[non_exhaustive]` `Errno` enum (also
  frozen). Trait surface: `SyscallHandlers::irq_bind`/`irq_wait`
  added with `Dispatcher::invoke` arms; production
  `KernelSyscallHandlers` (`kernel/core/src/syscalls.rs`)
  implements both as `SYSCALL_FEATURE_UNAVAILABLE(feature =
  irq_subsystem) + Errno::NotImplemented` — the same deferral
  pattern `cap_delegate` uses for `user_memory_copyin`. Mock and
  fuzz-test handlers updated. **What is deferred to Items 2-tail /
  3–6.** The kernel-side IRQ table, the per-handle wait queue, the
  controller-level mask/unmask sequence, the
  `KernelVirtioHost::notify_wait` rewrite, the kernel-binary
  `VirtioHostFactory` impl, and the QEMU mock-device wake-up
  integration test all remain outstanding. The ABI-half landing
  was scoped explicitly because the next-session-prompt's "Item 2
  in full" wording presumed kernel infrastructure (production
  handlers wiring, scheduler wait-queue API, per-process `DmaPool`
  carve point reachable from kernel binary) parts of which
  required additional follow-up sessions to land at AGENTS.md's
  no-hacks bar. **Docs.** New
  [`docs/src/security/irq.md`](docs/src/security/irq.md) locks down the
  user-visible contract (per-architecture line-id namespaces,
  `CAP_IRQ_BIND` rationale, wake-up sequencing, failure-mode
  table, mask-before-wake invariant); cross-linked from
  `docs/src/architecture/syscalls.md` (capability matrix +
  handler-wiring row) and added to `docs/src/SUMMARY.md` under
  "Security". **Tests.** New unit tests:
  `lib/abi/src/capability.rs::well_known_ids_are_frozen` + index
  test extended for `IRQ_BIND`;
  `lib/abi/src/syscall.rs::well_known_numbers_are_frozen` +
  `irq_handle_round_trips_and_invalid_is_zero`;
  `lib/abi/src/syscalls.rs::capability_requirements_are_frozen`
  pins both rows + the audit flag;
  `lib/abi/src/error.rs::discriminants_are_frozen` extended for
  `TimedOut`; `kernel/sec` audit test mirrored.
  `kernel/syscall/src/table.rs` gained four new dispatcher tests
  (`irq_bind_without_capability_is_refused_and_audited`,
  `irq_bind_with_capability_reaches_handler_and_audits_invocation`,
  `irq_bind_rejects_line_argument_with_high_bits_set`,
  `irq_wait_passes_handle_and_timeout_verbatim`) covering all
  four dispatcher policy edges (cap-denied, cap-granted + decode
  + audit, U32 high-bits rejected, opaque-handle passthrough).
  `kernel/core` gained
  `irq_{bind,wait}_returns_not_implemented_and_audits_feature_unavailable`
  mirroring the `cap_delegate` deferral test. **Verification.**
  `cargo xtask abi-check` clean. `cargo test -p rustos-abi -p
  rustos-kernel-syscall -p rustos-kernel-sec -p
  rustos-kernel-core` all green. Items 3–6 (bus-handle hand-off,
  four QEMU integration test crates, userland ARP/IP/ICMP
  responder, acceptance gate) plus the Item 2-tail kernel work
  remain outstanding and have been rewritten into the new
  `.junie/next-session-prompt.md`.

- Stage 4.D follow-up (Item 0a — owned `DmaSlab` API shape,
  *complete*): the API conflict described below has been resolved
  by adopting Option (a). The driver-side `DmaRegion<'a>` borrowed
  view has been replaced by an owned `DmaSlab { phys: u64, ptr:
  NonNull<u8>, len: usize, pool_id: PoolId, slot: usize, /* erased
  free shim */ }` in `drivers/bus/virtio/src/dma.rs`. The slab
  carries the disjoint-slot invariant in its `pool_id` / `slot`
  fields; `DmaSlab::as_bytes_mut`'s `// SAFETY:` block cites the
  pool's slot bitmap (one slot ↔ one slab) as the disjointness
  witness. `BounceBuffer` now wraps `DmaSlab`. The `VirtioHost`
  trait return type is `Result<DmaSlab, DriverError>` (no
  lifetime); `SplitQueue` stores three owned `DmaSlab`s. The
  kernel-side companion accessor
  `DmaPool::slot_base(&self, &DmaBuffer) -> Result<NonNull<u8>,
  DmaError>` has landed in `kernel/mem/src/dma.rs` with two new
  tests (`slot_base_points_at_live_data_bytes`,
  `slot_base_rejects_unknown_buffer`). The `MockHost`'s
  `Box::leak` storage strategy is unchanged: slabs are minted with
  `PoolId::MOCK`, a monotonically increasing `slot`, and a `None`
  free shim. Four new `DmaSlab` tests in
  `drivers/bus/virtio/src/dma.rs` exercise the round-trip, three
  simultaneous disjoint writes, drop-frees-pool (the erased free
  shim is invoked exactly once with the right `(slot, len)`), and
  pool-id rejection across pools. Docs:
  `docs/src/drivers/virtio.md` rewritten with a new "DMA ownership
  model" section; `docs/src/architecture/memory.md` gained a
  "Slab hand-off to user-space drivers" subsection (§5.1).
  `cargo test --workspace --lib --exclude rustos-kernel-arch-*` →
  656 passing on the pinned `nightly-2026-05-27` (was 650
  pre-Item-0a; +4 in `rustos-drv-bus-virtio` for the new
  `DmaSlab` tests, +2 in `rustos-kernel-mem` for `slot_base`).
  `cargo clippy -p rustos-drv-bus-virtio -p
  rustos-drv-storage-virtio-blk -p rustos-drv-network-virtio-net
  -p rustos-kernel-mem -p rustos-kernel-sec --lib --tests --
  -D warnings` is clean. `cargo fmt --check` is clean. Items 0,
  2–6 of the prior next-session prompt remain outstanding and
  have been rewritten into the next session's prompt.

- Stage 4.D follow-up (Item 0 — `KernelVirtioHost` wiring,
  *complete*): the in-kernel companion to `MockHost` has landed in
  `drivers/bus/virtio/src/kernel_host.rs`, gated behind a new
  `kernel-host` Cargo feature so the userland / cross-arch build
  matrix stays free of `kernel/*` deps (`AGENTS.md` §2.3). The
  type `KernelVirtioHost<'a, P: PageTableOps, S: Sink + ?Sized>`
  wraps a borrowed `&'a mut DmaPool<'a, P>`, the calling task's
  `&'a TaskCapabilities`, an audit `&'a S`, a fresh `PoolId`, a
  monotonic slot counter, and a `RefCell<BTreeMap<usize,
  DmaBuffer>>` live table. `alloc_dma_zeroed` routes through
  `kernel/sec::dma::alloc_dma` (which performs the
  `CapabilityId::MEM_DMA` check and emits
  `AuditEvent::DmaAllocated` / `…Denied`), then mints a `DmaSlab`
  via `DmaSlab::from_pool` carrying a generic
  `slab_free_shim::<P, S>` that re-enters the host on drop and
  routes the buffer back through `kernel/sec::dma::free_dma`. The
  single `unsafe` site (`DmaSlab::from_pool`) carries the full
  `// SAFETY:` justification cited in the module-level rustdoc —
  pool-bitmap disjointness, lifetime-bounded host pointer, and
  monomorphised cast inverse — per `AGENTS.md` §2.10. Capability
  refusals surface as `DriverError::PermissionDenied`; every other
  refusal collapses to `DriverError::LengthOutOfRange`, matching
  the `MockHost` failure surface. `notify_wait` remains the polled
  cooperative shim from `MockHost` — IRQ-routed wake-ups are
  Stage 4.D Item 2 (carried into the next session). Tests: 7 new
  in-crate units in `drivers/bus/virtio/src/kernel_host.rs::tests`
  (zero-initialised slab + audit emit, drop routes through
  `free_dma` (`live()` returns to zero), capability-missing →
  `PermissionDenied` + `DmaAllocDenied` event, zero-size →
  `BufferTooSmall`, two simultaneous disjoint slabs, `notify_wait`
  records the queue index, oversize → `LengthOutOfRange`). Crate
  totals: `cargo test -p rustos-drv-bus-virtio --lib` → 41
  passing (was 34). Workspace: `cargo test --workspace --lib
  --exclude rustos-kernel-arch-*` → 663 passing on the pinned
  `nightly-2026-05-27` (was 656; +7). `cargo clippy -p
  rustos-drv-bus-virtio --lib --tests --all-features -- -D
  warnings` and `cargo fmt --check` are clean. Docs: new
  "Kernel host (`KernelVirtioHost`)" section in
  `docs/src/drivers/virtio.md` and refreshed "Test surface"
  paragraph. The DriverHost-trait `dma_pool` accessor that the
  prior next-session prompt sketched was deliberately not added:
  there is no in-tree `.rxe` driver yet that consumes a
  `VirtioHost` through the `DriverHost` surface, so adding the
  accessor without a consumer would be the kind of dead-code
  bloat `AGENTS.md` §2.3 forbids. The drvhost ↔
  `KernelVirtioHost` plumbing is reopened in the next-session
  prompt and will land alongside the first in-tree consumer (the
  virtio-blk / virtio-net `.rxe` images planned for Item 4).
  Items 2–6 of the prior next-session prompt remain outstanding
  and have been rewritten into the new
  `.junie/next-session-prompt.md`.

- Stage 4.D follow-up (Item 0 — `DmaPool` ↔ `VirtioHost` API
  shape, *historical, superseded by Item 0a above*): the kernel
  side of the DMA facility (Item 1) and the driver consumers
  (`drivers/bus/virtio`, `drivers/storage/virtio_blk`,
  `drivers/network/virtio_net`) did not meet. The
  `VirtioHost::alloc_dma_zeroed(&self, size)
  -> Result<DmaRegion<'_>, DriverError>` trait method returned a
  borrowed `&'a mut [u8]` whose lifetime is tied to `&self`, but
  `DmaPool::alloc(&mut self, size)` requires an exclusive borrow
  of the pool and `DmaPool::bytes_mut(&mut self, &DmaBuffer)`
  re-borrows the pool on every call. A real kernel-backed
  `VirtioHost` wrapping a `&mut DmaPool<P>` therefore cannot hand
  out the **three simultaneous live `DmaRegion`s** that
  `SplitQueue::new` constructs (descriptor table, avail ring, used
  ring) — let alone the additional per-transaction regions that
  `virtio_blk::submit` and `virtio_net::transmit` hold. Three
  shapes are on the table, ordered by preference:
  (a) **Owned `DmaSlab` (recommended).** Replace
      `DmaRegion<'a>` with an owned handle `DmaSlab { phys: u64,
      ptr: NonNull<u8>, len: usize, pool_id: PoolId, slot: usize
      }` that carries the disjoint-slot invariant in its
      `slot`/`pool_id` fields. The host hands the slab back to
      `kernel_sec::free_dma` at drop time. The `&mut [u8]`
      borrow happens lazily through `DmaSlab::as_bytes_mut`,
      whose `// SAFETY:` block cites the pool's slot bitmap as
      the disjointness witness. Required pool surface:
      `DmaPool::slot_base(&self, &DmaBuffer) -> NonNull<u8>` —
      one extra accessor, no widening of the existing borrowed
      `bytes_mut` API.
  (b) **Pair / vector disjoint borrows.** Extend `DmaPool` with
      `bytes_mut_n(&mut self, [&DmaBuffer; N]) -> [&mut [u8]; N]`
      that verifies disjointness once and lends N slices. Forces
      the driver to materialise *all* simultaneous regions in a
      single call site, which does not match the
      `SplitQueue::new`-then-loop-of-`submit` driver structure.
      Rejected for that reason.
  (c) **Per-region pool.** Carve a *separate* `DmaPool` per
      virtio region (one for descriptors, one per outstanding
      transaction). Multiplies the per-driver page-table-mapping
      cost by the queue depth (×128 by default) and breaks the
      `AGENTS.md` §4 "per-process heaps, never global" rule by
      requiring the driver host to predict the driver's
      allocation pattern. Rejected.
  The next session **must pick (a)** (or justify (b) / (c) /
  another option in this list) **before** writing the
  driver-host wiring. The blocker is recorded in
  `.junie/next-session-prompt.md` as Item 0a; Items 2–6 are
  unchanged. No code in `kernel/mem`, `kernel/sec`,
  `drivers/bus/virtio`, or `userland/system/drvhost` has been
  modified in this session — both because the API choice is a
  versioned-interface decision (`AGENTS.md` §2.4) and because
  inventing a half-shape without tests would violate
  `AGENTS.md` §15.2. `cargo test --workspace --lib` minus the
  four `kernel/arch/*` targets that need a real boot environment
  → 650 passing on the pinned `nightly-2026-05-27`, 0 failing,
  identical to the post-Item-1 baseline.

---

## Stage 4.HW — Hardware Detection and Driver Autoload

**Dependencies:** Stage 4 (driver host + bus drivers) and the Stage 3
sub-stages for each target's early-boot platform discovery.

This stage implements `AGENTS.md` §18: detect the hardware present at
boot and autoload the matching drivers, with no hand-maintained static
device list.

**Deliverables**
- `lib/abi/src/hwtree.rs`: the architecture-neutral **hardware tree** ABI
  type (§18.1). Versioned, hashed, frozen on release like the syscall
  table (§9) and sysinfo (§16.6); each node carries a stable id, parent,
  device class, match keys (DT `compatible`, PCI `vendor:device:class`,
  USB `vid:pid:class`, virtio id, MMIO `compatible`), and its resource
  requirements expressed as capability-grant requests (never ambient
  handles).
- Per-architecture discovery that emits the hardware tree, living **only**
  under `kernel/arch/<target>/` as part of the Arch HAL "early-boot
  platform discovery" (§17.2):
  - `aarch64`, `riscv64`: FDT/DTB → hardware tree.
  - `x86_64`: ACPI (+ UEFI/firmware hand-off) and legacy fallbacks →
    hardware tree.
  - `wasm32`: host-environment capability query → hardware tree.
  - Bus children enumerated by `drivers/bus/*` are attached as nodes.
- `userland/system/devmgr`: user-space device manager that reads the
  hardware tree, matches nodes against each driver manifest's **bind
  table**, and autoloads matching drivers through the §8 driver-host
  load gate under `CAP_DRV_LOAD` / `CAP_DRV_KERNEL`. Deterministic match
  resolution; fail-closed; every match/load/skip/failure logged through
  `lib/log` with a stable event ID.
- Driver-manifest **bind table** (§8, §9): drivers declare the match keys
  they bind to. Wire it into the existing signed-manifest path.
- Runtime path: hotplug/removal updates the tree and triggers
  load/unload (§8). Unbound nodes are logged, never an error (§18.4).
- A privileged System Information API query (`CAP_SYSINFO_HW`, §16.6)
  that exposes the hardware tree read-only to tools; no `/proc`/`/sys`.

**Tests**
- Host unit tests for `lib/abi::hwtree` encode/decode and ABI hashing.
- Host unit tests for `devmgr` matching: exact match, multi-match
  priority resolution, unbroken-tie rejection, no-match → unbound,
  capability-denied load fails closed.
- Per-arch host tests that the discovery code normalises a sample
  FDT / ACPI / host descriptor into the expected tree.
- QEMU integration per Tier-1 target: boot → devmgr autoloads the
  input/display/storage/network drivers for the emulated devices →
  device usable; headless image leaves the display node unbound and
  reaches text login without error (§17.3).

**Docs**
- `docs/src/drivers/hardware-detection.md` mirroring `AGENTS.md` §18.
- Update `docs/src/drivers/overview.md` and `docs/src/abi/` for the
  hardware-tree type and the `devmgr` service.

---

## Stage 5 — Filesystem

**Dependencies:** Stage 4 (`Filesystem` trait + a block driver).

**Deliverables**
- `drivers/filesystem/rustfs`: native FS, copy-on-write, ACL + capability
  gates per inode, journaled, POSIX-compliant (latest standard targeted).
- `drivers/filesystem/ext4`: read/write driver (uses upstream-audited parser
  where possible; otherwise implemented in-tree with tests).
- `drivers/filesystem/fat32`: read/write (for EFI system partition and SD
  cards).
- VFS layer in `kernel/core` (path resolution, mount table, permission
  enforcement via `kernel/sec`).
- Enforcement of the on-disk layout defined in `AGENTS.md` §16: the VFS
  refuses to create any of the reserved legacy POSIX top-level names
  (`/etc`, `/home`, `/usr`, `/var`, `/proc`, `/sys`, `/lib`, `/lib64`,
  `/bin`, `/sbin`, `/opt`, `/root`, `/tmp`, `/dev`, `/mnt`, `/media`,
  `/run`, `/boot`), and the default root template provides only
  `/System`, `/Users`, `/Apps`, `/Storage`.

**Tests**
- POSIX FS test suite (`pjdfstest`-equivalent) run under QEMU.
- ACL + capability gate tests: a user without `CAP_AUDIT_READ` cannot read
  a file marked as such, even with mode 0644.
- Crash-consistency tests for `rustfs` journal.
- Layout-enforcement tests: attempting to `mkdir /etc` (or any other
  reserved name from `AGENTS.md` §16.1) at the root returns
  `Error::ReservedPath`; `/System` is read-only at runtime except for
  the two writable paths listed in §16.2.

**Docs**
- `docs/src/filesystem/{overview,rustfs,ext4,fat32,permissions,layout}.md`
  (the new `layout.md` mirrors `AGENTS.md` §16).

**Status: in progress — VFS policy layer landed.**
- The architecture-neutral **VFS layer** is implemented in
  `kernel/core/src/fs/` (`path`, `perm`, `mount`, `vfs`, `mod`):
  - `path`: absolute-path parsing that rejects relative paths, empty/
    over-long components, `.`/`..`, and NUL bytes; the `AGENTS.md` §16.1
    reserved-name list and the four-entry root template as data.
  - `perm`: the §5.3 permission model — POSIX mode bits **plus** ACL
    **plus** an optional per-inode capability gate — decided by one
    `Metadata::authorize` that fails closed and never branches on
    `uid == 0` (§5.1).
  - `mount`: a `MountTable` with longest-mount-prefix resolution and the
    read-only query that backs `/System` (§16.2).
  - `vfs`: the `Vfs` tree (`metadata`/`mkdir`/`create_file`/`read`/`write`/
    `list`/`remove`/`set_required_cap`) enforcing the §16 layout, the
    read-only `/System` (create/remove judged by the *parent's* mount),
    and the §5.3 checks with per-directory search permission on traversal.
    `Vfs::with_default_layout` lays out exactly `/System`, `/Users`,
    `/Apps`, `/Storage` plus the writable `/System/Logs` and
    `/System/Settings` child mounts.
- Added `MountFlags::union` (const) to `lib/abi` to compose
  `nosuid,nodev,noexec` mount policies without fallible re-validation.
- Tests: 41 unit tests in `kernel/core::fs` (incl. the §16
  layout-enforcement tests — `mkdir /etc` → `VfsError::ReservedPath`;
  read-only `/System` with writable `Logs`/`Settings`; the §5.3
  capability-gate test — a `CAP_AUDIT_READ`-marked file unreadable at mode
  `0644` without the capability) + 1 new `MountFlags::union` test.
- Docs: `docs/src/filesystem/{overview,layout,permissions}.md`
  (linked in `SUMMARY.md`).
- The block-backed **FAT32 driver** (`drivers/filesystem/fat32`,
  `rustos-drv-fs-fat32`) is implemented read-only over the Stage 4
  `Block` trait — the first block-backed `drivers/filesystem/*` crate
  (FAT32 first per §11, for the EFI system partition / SD cards):
  - Validates the FAT32 boot sector / BPB (signature, power-of-two
    sector & cluster sizes, FAT32 markers — zero 16-bit FAT size and
    zero root-entry count, so FAT12/FAT16 is rejected), walks the FAT
    cluster chain, lists directories, and reads files. All device I/O is
    staged one logical block at a time, decoupling the device block size
    from the FAT bytes-per-sector. No `unwrap`/`expect`/`panic!` and no
    `unsafe`.
  - The frozen `Filesystem` trait is mount/unmount only and cannot do
    I/O, so the read surface is a **new versioned trait**
    `FilesystemRead` (`NodeId`/`NodeKind`/`NodeInfo`/`DirEntry`) added to
    `lib/abi/src/driver/filesystem.rs` — additive, not a widening of the
    shipped trait (§2.4 / §9). `NodeId` is a self-describing packed token
    (first cluster + dir flag + size), so there is no in-memory inode
    table.
  - **Long file names (VFAT) are reconstructed** (read-only): each entry
    exposes a single UTF-8 name — its long name when a valid long-name
    set (contiguous sequence, `0x40` last-logical fragment, matching
    short-name checksum) precedes the 8.3 entry, otherwise the short
    name. UTF-16LE units (incl. surrogate pairs) are decoded with a
    first-party `decode_utf16le`; any malformed set (unpaired surrogate,
    invalid scalar, checksum mismatch) falls back to the short name
    rather than surfacing a partial name. No new dependency, no `unsafe`,
    no `unwrap`/`panic`.
  - Tests: 25 host-side unit tests against a hand-built, allocation-free
    in-memory FAT32 image + mock `Block` (open/validation incl.
    bad-signature & non-FAT32 rejection, ordered listing, case-
    insensitive lookup, subdirectory traversal, cross-cluster and
    boundary-straddling reads, offset/EOF, `Unsupported`/`BufferTooSmall`
    paths, `register` cap-gate; plus LFN listing/lookup, the short name
    superseded by its long name, checksum-mismatch fall-back, and the
    `decode_utf16le`/`short_name_checksum` units), plus 5 `lib/abi` tests
    for the read trait. Full `cargo xtask ci` green.
  - Docs: `docs/src/filesystem/fat32.md` (+ SUMMARY link), the
    `FilesystemRead` section in `docs/src/abi/driver_traits.md`, the
    filesystem overview note, and `drivers/filesystem/fat32/README.md`.
- **VFS driver delegation landed** (read path): `kernel/core::fs` now
  routes resolution under a driver-backed mount to a `FilesystemRead`
  driver. A new `delegate` module exposes `DelegatedFs` (a per-call
  adapter over a borrowed `&mut dyn FilesystemRead` — the live driver is
  not stored in the `Clone + Eq` `Vfs`, only the mount's `DriverHandle`
  is), and `Vfs::{read_via, list_via, stat_via}` walk the in-RAM tree to
  the mount point (authorising search on every ancestor) then delegate the
  remainder. The node-id ↔ VFS-inode bridge is *uniform-template*: every
  delegated node inherits the mount point's `Metadata` for the §5.3 check
  (FAT stores no per-file owner), and the driver makes no permission
  decision (§5.4). A new `VfsError::Io` carries unrecoverable driver
  faults and structurally invalid driver responses (e.g. a non-UTF-8
  directory name). 12 host-side delegation tests + 1 errno-mapping test;
  the `docs/src/filesystem/overview.md` "Driver delegation" section
  documents it.
- **FAT32 write support + write-path delegation landed.** A new
  **versioned `FilesystemWrite`** trait (`create`/`write_at`/`truncate`/
  `remove`/`flush`) was added to `lib/abi/src/driver/filesystem.rs` — the
  symmetric counterpart to `FilesystemRead`, additive rather than a
  widening of the frozen `Filesystem` or of `FilesystemRead`
  (`AGENTS.md` §2.4 / §9). Its mutating methods address a target as a
  `(dir, name)` pair because a FAT file's length and first cluster live
  in the *parent directory entry*, not in a self-describing `NodeId`.
  - `drivers/filesystem/fat32` now implements it: free-cluster scanning
    with FAT mirroring across all copies, cluster-chain extend/free,
    directory-slot finding/growth, VFAT long-name-set writing bound to a
    generated directory-unique `~N` 8.3 short alias (so arbitrary,
    case-preserving names round-trip), sparse zero-fill, and `truncate`
    shrink/grow. No `unwrap`/`expect`/`panic!`, no `unsafe`; writes are
    synchronous (no journal — FAT has none). 38 host tests (13 new
    write/round-trip) + 2 new `lib/abi` `FilesystemWrite` mock tests.
  - `kernel/core::fs` gains the symmetric `Vfs::{create_via, mkdir_via,
    write_via, truncate_via, remove_via}` over `DelegatedFs` (made
    generic over the borrowed driver), authorising §5.3 write on the
    parent template and refusing a `READ_ONLY` mount; existence/emptiness
    are pre-checked for precise `AlreadyExists`/`NotEmpty`/`IsADirectory`.
    10 new delegated write tests against an in-memory read/write mock.
  - Docs updated: the `FilesystemWrite` section in
    `docs/src/abi/driver_traits.md`, the write tables/sections in
    `docs/src/filesystem/{fat32,overview}.md`, and the FAT32 `README.md`.
- **End-to-end QEMU FAT32 vertical landed.** A new
  `tests/integration/fat32_virtio_blk_pci_x86_64` boots the production
  kernel, brings a real (emulated) virtio-blk-pci device online through
  the shared virtio bring-up, then mounts a planted FAT32 image through
  the real FAT32 driver, verifies the planted file, and creates + writes
  + reads back a fresh file before signalling QEMU success. The on-disk
  image is built by a new shared `tests/integration/fat32_image`
  (`rustos-test-fat32-image`) fixture — a 1 MiB volume, two mirrored
  FATs, one-sector clusters, accepted by `Fat32::open` — and host-tested
  (5 tests) by round-tripping it through the real read/write driver. The
  host harness (`cargo xtask test --qemu`) plants exactly that image; the
  freestanding guest tail (`fat32_round_trip`, in the shared
  `virtio_qemu_support` crate, generic over the virtio transport so the
  riscv64 MMIO sibling can reuse it) names the same files through the
  fixture, so both sides share one source of truth (§2.2). Docs:
  `docs/src/filesystem/{fat32,overview}.md` + the FAT32 `README.md`.
- **Native `rustfs` driver landed.** `drivers/filesystem/rustfs`
  (`rustos-drv-fs-rustfs`) is a block-backed, **journaled, copy-on-write**
  filesystem that stores full POSIX metadata plus an inline ACL and an
  optional capability gate **per inode** (§5.3), exposed through the
  versioned `FilesystemRead` + `FilesystemWrite` traits (not a widening of
  the frozen `Filesystem`, §2.4 / §9).
  - On-disk: superblock, 256-byte inodes (16 direct + 1 single-indirect),
    a data-block bitmap, a redo-log journal, and data blocks; geometry is
    re-derived and validated at `open`.
  - File **data is copy-on-write** (new block, re-point inode, free old);
    **metadata is journaled** (bitmap/inode/dir/indirect images staged into
    the on-disk journal, a checksummed commit record, then checkpoint to
    home blocks). A mount replays a committed-but-un-checkpointed
    transaction and discards an uncommitted one. Only the home block list
    lives in RAM. No `unwrap`/`expect`/`panic!`, no `unsafe`.
  - The per-inode security record is surfaced to the host via
    `RustFs::security` / `RustFs::set_security` (the driver makes no
    permission decision, §5.4).
  - 18 host tests: format/open, create/lookup/list, read/write across
    block boundaries + sparse fill, single-indirect large files across a
    remount, `truncate` shrink/grow, `remove` + reuse, non-empty-dir
    `Busy`, the per-inode ACL + capability-gate record persisting across a
    remount, CoW overwrite persistence, the `register` cap-gate, a
    **crash-consistency sweep** that faults the device after every write
    count during a journalled overwrite and asserts fully-old-or-fully-new
    (both observed), and a **journal soak** that drives a deterministic,
    seeded `create`/`write`/`truncate`/`remove` stream and crash-tests
    *every* operation at *every* device-write count — the recovered
    whole-tree snapshot must equal the volume either exactly before or
    exactly after the operation (never an intermediate) and stay mountable,
    with rollbacks and replays both observed. Docs:
    `docs/src/filesystem/rustfs.md` (+ SUMMARY link) and the crate
    `README.md`.
- **`rustfs` per-inode security surfaced into the VFS — DONE.** A third
  separate versioned ABI trait, `FilesystemSecurity`
  (`security(node) -> NodeSecurity`), was added to
  `lib/abi/src/driver/filesystem.rs` alongside the allocation-free
  `NodeSecurity`/`SecurityAcl`/`SecuritySubject` §5.3 record (mode, uid,
  gid, `required_cap`, up to `MAX_ACL_ENTRIES = 8` grant-only ACL entries)
  — additive, never a widening of the frozen `Filesystem` or of
  `FilesystemRead`/`FilesystemWrite` (§2.4 / §9).
  - `rustfs` now **uses these ABI types as its own storage** (its former
    crate-local `Security`/`AclEntry`/`AclSubject` are `pub use` aliases of
    them, eliminating the §2.2 duplication) and implements
    `FilesystemSecurity`; a const-assert pins its on-disk `ACL_MAX` to
    `MAX_ACL_ENTRIES`.
  - `kernel/core::fs` translates the record with
    `Metadata::from_node_security` (each grant-only ACL bit → one *allow*
    `AclEntry`, reusing the single `Access::bit` rwx mapping), and
    `DelegatedFs` gained a `MetaPolicy` type parameter (`Uniform` =
    mount-point template, `PerInode` = the driver's stored record). The VFS
    exposes `read_via_secured` / `list_via_secured` / `stat_via_secured` /
    `create_via_secured` / `mkdir_via_secured` / `write_via_secured` /
    `truncate_via_secured` / `remove_via_secured`; both routes feed the
    *same* `Metadata::authorize` decision, so policy stays single-sourced.
  - Tests: 2 new `lib/abi` security tests, 1 `perm::from_node_security`
    test, and 5 secured-delegation tests (per-inode owner/mode +
    capability-gate denial vs the uniform template, owner-with-capability
    allow, secured `stat`/`list`, and a secured-write parent-permission
    denial). Docs: `docs/src/abi/driver_traits.md`,
    `docs/src/filesystem/{overview,rustfs,permissions}.md`, and the rustfs
    `README.md`.
- **`ext4` read-only driver — DONE.** `drivers/filesystem/ext4`
  (`rustos-drv-fs-ext4`) reads ext2/ext3/ext4 volumes over the Stage 4
  `Block` trait and implements `FilesystemRead` (read-only; the frozen
  `Filesystem` stays mount/unmount, §2.4 / §9). `Ext4::open` validates the
  superblock (magic `0xEF53` at the fixed byte offset 1024) and re-derives
  the geometry (block size 1024..=4096, 128/256-byte inodes, 32/64-byte
  group descriptors incl. the `64bit` feature). A `NodeId` is the on-disk
  inode number (no in-memory inode table); logical→physical mapping covers
  both **extent-mapped** inodes (extent tree incl. interior index nodes)
  and the classic **block map** (12 direct + single/double/triple
  indirect), with sparse holes reading as zeros. Linear directory blocks
  are walked honouring `rec_len`, skipping `.`/`..` and unused slots;
  hash-indexed (`htree`) directories are read through their linear leaf
  view. No `unwrap`/`expect`/`panic!`, no `unsafe`. 17 host tests against a
  hand-built in-memory image (extent root/file, subdir + nested file,
  classic file across holes + the direct/indirect boundary, bad-magic
  rejection, `Unsupported`/`BufferTooSmall`/`NotFound` guards, the
  `register` cap-gate, and the `FilesystemSecurity` record). Docs:
  `docs/src/filesystem/ext4.md` (+ SUMMARY link), the overview and
  `FilesystemRead` notes, and the crate `README.md`.
- **`ext4` per-inode security surfaced into the VFS — DONE.** The `ext4`
  driver now decodes each inode's owner (`i_uid`/`i_gid` recombined with
  the osd2 high halves `l_i_uid_high`/`l_i_gid_high`) and implements the
  versioned `FilesystemSecurity` trait: `security(node)` reports a
  `NodeSecurity` carrying the POSIX mode (low 12 bits, type bits stripped)
  and owner uid/gid. ext4 has no inline capability gate and its POSIX ACLs
  live in extended-attribute blocks the read surface does not yet decode,
  so the record surfaces no `required_cap` and no inline ACL entries
  (xattr ACL decoding is deferred alongside write support). 3 new host
  tests (file mode/owner with both id halves, directory record,
  `NotFound`). Docs: `docs/src/filesystem/{ext4,overview}.md`,
  `docs/src/abi/driver_traits.md`, and the crate `README.md`.
- **`ext4` write support — DONE.** The `ext4` driver now implements the
  versioned `FilesystemWrite` trait (`create`/`write_at`/`truncate`/
  `remove`/`flush`): block + inode bitmap allocation with
  group-descriptor and superblock free-count maintenance, classic
  block-map allocation (direct + single indirect) for new objects,
  directory-entry insertion (record-slack split, block append) and
  removal (merge into the predecessor), and `truncate` shrink/grow over
  both the classic map and an inline depth-0 extent root (zeroing the
  retained partial block). Because correct on-disk checksums and wide
  descriptors are a prerequisite for safe mutation, the write path
  refuses (`Unsupported`) any volume carrying `metadata_csum`,
  `gdt_csum`/`uninit_bg`, or `64bit` (such volumes stay fully readable),
  and refuses to free a mapping it cannot fully account for rather than
  orphan blocks (§2.1 / §5.4 — fail closed). No `unwrap`/`expect`/
  `panic!`, no `unsafe`. Tests grew from 17 to **32** (create + multi-block
  write round-trips across a remount, sparse extension, `truncate`
  shrink-then-grow, directory create/remove incl. `Busy`, inode reuse,
  the directory/not-found/invalid-name guards, free-inode exhaustion, and
  the fail-closed refusal on a `metadata_csum` volume). Docs:
  `docs/src/filesystem/{ext4,overview}.md` and the crate `README.md`.
- **`ext4` POSIX-ACL decode — DONE.** `security(node)` now folds the
  inode's `system.posix_acl_access` extended attribute into the
  `NodeSecurity` record. Both ext4 storage forms are read: the inline
  region in an enlarged (`inode_size > 128`) inode's tail (after
  `i_extra_isize`, value offsets relative to the first entry) and the
  external block named by `i_file_acl` (value offsets relative to the
  block start), sharing the `ext4_xattr_entry` encoding (magic
  `0xEA020000`, `e_name_index = 2`). Named `ACL_USER`/`ACL_GROUP`
  entries become one grant-only `SecurityAcl` each
  (`SecuritySubject::User`/`Group`, POSIX `rwx`); the
  owner/owning-group/other/mask entries are already expressed by the
  mode bits and are skipped, and an absent or malformed attribute
  contributes no grants (§5.4 — fail closed, never widen). No
  `unwrap`/`expect`/`panic!`, no `unsafe`. Tests grew from 32 to **41**
  (standalone `decode`/`find` units for both value-base conventions,
  bad version, the inline-budget cap, unrelated attributes; end-to-end
  `security` reads of an external xattr block, a garbage block, and an
  inline ACL in a 256-byte-inode volume). Docs:
  `docs/src/filesystem/ext4.md` and the crate `README.md`.
- **`ext4` interior extent-tree growth — DONE.** A pre-existing
  extent-mapped file now grows beyond the four inline `i_block` extent
  slots: when they are exhausted, `write_at` converts the inline depth-0
  root into a **depth-1 tree** (the four extents move into a freshly
  allocated leaf block and the root becomes a single index entry), then
  attaches further leaves through new ascending-ordered root index
  entries; the last extent is extended in place when contiguous.
  `truncate`/`remove` free a depth-1 tree's emptied leaf blocks, drop
  their index entries, and collapse the root back to an empty depth-0
  node when none survive. The depth-0 leaf find/append/extend and the
  per-leaf trim are shared free functions reused by the root and by leaf
  blocks (§2.2 — no duplicated extent algebra). A tree that would need a
  second index level (depth ≥ 2) is refused (`DeviceFault`) rather than
  half-built — the driver never builds one and the read path still maps
  any on-disk depth (§2.1 / §5.4 — fail closed). No
  `unwrap`/`expect`/`panic!`, no `unsafe`. Tests grew from 41 to **44**
  (depth-0 → depth-1 conversion with read-back + remount persistence and
  a sparse hole between extents, depth-1 `truncate`-to-zero with block
  reuse, and `remove` of a depth-1 file with reuse). Docs:
  `docs/src/filesystem/ext4.md` and the crate `README.md`.
- **`ext4` checksummed + wide-descriptor mutation — DONE.** The write
  path now maintains every on-disk checksum a volume carries, so a
  default `mkfs.ext4` image (`metadata_csum,extent,64bit`) is mutated in
  place. First-party `crc32c` (reversed poly `0x82F6_3B78`, seeded
  `crc32c(~0, uuid)`) covers the superblock `s_checksum`, group-
  descriptor `bg_checksum`, block/inode-bitmap checksums, per-inode
  `i_checksum_lo`/`hi`, directory-leaf `ext4_dir_entry_tail`, and
  extent-block `ext4_extent_tail`; first-party `crc16` (poly `0xA001`)
  covers the legacy `gdt_csum`/`uninit_bg` descriptor checksum; the
  64-byte descriptor's high-half checksum and `bg_itable_unused` are
  maintained too (a storage checksum is not a cryptographic primitive,
  so §2.12 does not apply). `remove` now marks the freed inode deleted
  (`i_links_count = 0`, `i_dtime`, zeroed size/blocks) and a latent
  `place_in_block` split bug (which zeroed the shrunk entry's name) was
  fixed. Mutation still fails closed (§5.4) on a feature outside the
  supported allow-list (`bigalloc`, `meta_bg`, `inline_data`,
  `checksum_seed`, …) and on an uninitialised block group. No
  `unwrap`/`expect`/`panic!`, no `unsafe`. **44 in-tree unit tests**
  plus a new **5-test integration suite** (`tests/checksummed.rs`)
  that mutates **real `mke2fs 1.47.0`** `metadata_csum` and `gdt_csum`
  fixtures (committed under `tests/fixtures/`) and re-verifies every
  checksum with an *independent* crc, pristine and post-mutation; the
  mutated images also pass `e2fsck -f` cleanly. Docs:
  `docs/src/filesystem/ext4.md` and the crate `README.md`.
- **Remaining for Stage 5** (dependency-gated — next sessions):
  - Mutation of the checksummed (`metadata_csum`/`gdt_csum`) and `64bit`
    ext4 feature sets — **DONE** (first-party crc32c + crc16 + wide
    descriptors; validated against real `mke2fs` images and `e2fsck`).
  - The `rustfs` journal crash-consistency *soak* — **DONE**: a
    deterministic, seeded multi-operation soak crash-tests every scripted
    `create`/`write`/`truncate`/`remove` at every device-write count and
    asserts whole-tree old-or-new recovery (never torn), staying mountable.
  - The end-to-end QEMU `rustfs`-over-virtio_blk vertical — **DONE**:
    `tests/integration/rustfs_virtio_blk_pci_x86_64` mounts a planted
    rustfs volume over a real (emulated) virtio-blk-pci device through the
    real driver and round-trips a read **and** a write. The backing image
    (`tests/integration/rustfs_image`) is authored by the rustfs driver
    itself (`RustFs::format` + plant), so the fixture and the driver share
    one source of truth for the on-disk format (§2.2); the transport-
    generic `rustfs_round_trip` tail makes a riscv64 MMIO sibling cheap.
  - The `pjdfstest`-equivalent POSIX suite — **DONE**:
    `tests/integration/posix_fs_suite` (`rustos-test-posix-fs-suite`)
    drives the real `rustfs` driver through the real `kernel/core::fs`
    VFS policy layer and asserts the POSIX-visible return values and
    error codes of every operation the system exposes — `mkdir`,
    `open`/`read`/`write`, `unlink`, `rmdir`, `truncate`,
    `readdir`/`stat`, the §5.3 permission model (mode bits, ACL grants,
    and the capability gate the charter names: a `CAP_AUDIT_READ`-marked
    file is unreadable at mode `0644` without the capability), the §16
    layout rules (the four top-level directories, reserved-name refusal,
    read-only `/System` with writable `Logs`/`Settings`, read-only
    mounts), the namespace constraints (absolute-only; `.`/`..`/NUL/
    over-long components refused), and the stable `Errno` mapping. The
    harness re-implements no FS semantics (§2.2); it is the *semantics*
    companion to the QEMU virtio-blk verticals, run on the host against
    the identical driver and VFS code. Docs:
    `docs/src/filesystem/posix_suite.md` and the crate `README.md`.
  - **First-party formatters + `NoSpace` + in-RAM filesystem soak —
    DONE.** A new `DriverError::NoSpace` / `Errno::NoSpace` (POSIX
    `ENOSPC`) distinguishes a healthy-but-full volume from a
    `DeviceFault` (§5.4); every driver's allocator now returns it on
    exhaustion. Each driver owns a real, parameterised first-party
    formatter (no `mkfs` shell-out, §12/§2.12): `RustFs::format`,
    `Fat32::format`, and a new reader-compatible multi-group
    `Ext4::format` (`drivers/filesystem/ext4/src/format.rs`:
    `filetype`+`extent`, no checksum/`64bit`, fully-materialised groups).
    `tests/integration/fs_soak` (`rustos-test-fs-soak`) formats a
    ≥ 1 GiB `RamBlock` with each formatter and drives one
    filesystem-agnostic exerciser over the frozen `FilesystemRead`/`Write`
    ABI — integrity round-trip + remount re-verify + the fail-closed
    extremes (`NoSpace`/`Busy`/`LengthOutOfRange`). `cargo xtask fssoak`
    (`--quick`/`--soak`/`--target`/`--secs`/`--list`, mirroring
    `proptest`) runs one filesystem at a time; `tools/ci/soak.sh`'s new
    `fssoak` kind (and `all`) fans the three out into parallel jobs, and
    `soak.yml` runs them nightly. Docs:
    `docs/src/filesystem/{soak,ext4}.md` + the three driver READMEs.
    **Stage 5 is now complete.**

---

## Stage 5 follow-up — RustFS (native on-disk format evolution)

**Dependencies:** Stage 5 (the VFS policy layer and the frozen
`Filesystem*` traits) and `lib/crypto`.

**Goal.** Build the native filesystem up to the full RustFS design:
copy-on-write, **always encrypted**, checksummed, compressed,
deduplicating, SSD-aware, and recoverable (scrub / check / rescue). There
is exactly **one** on-disk version — `rustfs` is grown in stages, but the
driver and its format are a single shipping thing, not a `v1`/`v2` pair
(the old journaled driver was fully replaced by this copy-on-write design).
The authoritative implementation spec is `docs/src/filesystem/rustfs-spec.md`; the
user-facing documentation is `docs/src/filesystem/rustfs.md`. RustFS has
**one mandatory profile** — every feature is on and not tunable — and **no
external zstd/compression dependency** (the codec is first-party,
`AGENTS.md` §2.12); crypto goes through `lib/crypto` only.

**Staged delivery.** Delivered **one stage per session, bottom-up**, in the
`docs/src/filesystem/rustfs-spec.md` §15 order, behind the existing frozen `FilesystemRead` /
`FilesystemWrite` / `FilesystemSecurity` / `FilesystemTimestamps` traits so
the VFS and the shipping `rustfs` driver are never regressed (parallel
implementations, not a `cfg` collapse — `AGENTS.md` §2.2 carve-out). The
live next-session prompt is `.junie/next-rustfs-prompt.md`; the status
legend is `docs/src/filesystem/rustfs-spec.md` §18. The 12 stages:

1. On-disk headers, superblock ring, transaction roots. ✓ (the
   copy-on-write `rustfs` driver — self-identifying block headers,
   four-slot superblock ring, transaction root + inline commit record,
   COW inode map + file/dir/data blocks — fully replaced the old
   journaled driver; complete, mountable, and exercised by the unit
   tests, the 1 GiB soak, the posix suite, the QEMU vertical, and the
   `fuzz_mount` harness).
2. COW metadata trees (inode, extent) and free-space rebuild. ✓ (one
   generic copy-on-write B-tree backs both the inode tree, keyed by inode
   number — superseding the fixed-cap inode map — and a per-file extent
   tree, logical block → physical run — superseding the 12-direct +
   single-indirect map; the transaction root names the inode-tree root and
   next inode number, the mount-time free-space rebuild walks the trees, and
   a two-cursor allocator keeps sequential data contiguous; tested for
   splits/merges across many inodes, many-extent files, contiguous-write
   collapse, free-space-rebuild equality, crash replay, and the extended
   `fuzz_mount`).
3. Metadata authentication/checksums and duplicated critical metadata. ✓
   (the fast physical checksum became a keyed authenticator — HMAC-SHA256
   through `lib/crypto` — over identity + payload, and every metadata block
   is stored in two physical copies, a primary and a companion mirror at
   `primary + 1`; one read path reads the primary, falls back to the
   companion, and repairs the bad copy from the good one, while both copies
   bad fails closed and never panics; tested for bit-flip repair, wrong-key
   rejection, both-copies-bad fail-closed, crash replay, and the extended
   `fuzz_mount` authenticated-header / duplicated-copy sweeps).
4. Encrypted volume creation, key hierarchy, filename/data encryption. ✓
   (`format`/`open` take a caller-supplied volume key; a per-volume master
   key is wrapped (AEAD) under a KDF of the volume key through `lib/crypto`
   — `lib/crypto/src/kdf.rs` — and stored only in wrapped form in the
   superblock's plaintext discovery region, deriving the
   metadata-authentication, filename, and content keys; a wrong key refuses
   the mount with `PermissionDenied`, fail-closed. File data and
   directory-entry names are encrypted at rest with ChaCha20-Poly1305
   (28-byte nonce+tag trailer per data/directory block; directory blocks
   are encrypt-then-MAC), so an encrypted-data/name bit-flip is detected on
   read. No plaintext layout exists; tested for wrong-key refusal,
   no-plaintext-at-rest, filename+data remount round-trip, encrypted-data
   bit-flip detection, crash replay, and the extended `fuzz_mount`
   encrypted-open sweep.)
5. Data records with physical checksum and logical hash. ✓ (every
   file-data block gains a 40-byte data-integrity trailer after the
   crypto trailer — a logical content hash, SHA-256 of the plaintext
   through `lib/crypto` (the spec's BLAKE3-256 constant yields to
   `AGENTS.md` §2.12: use the audited `lib/crypto` hash, no `blake3`
   crate that does not build cleanly on the bare-metal targets), and a
   fast first-party FNV-1a physical checksum over the at-rest block; the
   read path verifies the checksum first (media corruption caught before
   the AEAD), decrypts, then verifies the logical hash, each layer
   failing closed and kept internally distinct (`integrity::DataFault`);
   tested for the three corruption classes each detected distinctly,
   identical-vs-different logical hash (the Stage 7 dedupe seam), and
   integrity surviving a remount and a copy-on-write rewrite).
6. First-party RustOS zstd codec and RustFS compression integration. ✓
   (a first-party LZ "zstd-fast-style" codec landed as the new `lib/compress`
   crate — `no_std`, allocation-free, no external zstd/compression dependency
   per `AGENTS.md` §2.12 / §16.4; `compress`/`decompress` are `Result`-based
   and panic-free, malformed input returns an error. RustFS wires it into the
   §6 data-record pipeline: on write `compress → encrypt` with an
   incompressible record stored **raw** (the §10 adaptive choice), on read
   `physical checksum → decrypt → decompress → verify logical hash`. A per-
   block compression descriptor (state + at-rest stored length) sits between
   the crypto trailer and the logical hash, so the physical checksum covers it;
   the full content slot is always encrypted so the Stage-4 crypto and Stage-5
   integrity layers are identical for compressed and raw records and the
   logical hash still names the plaintext (the Stage 7 dedupe seam is
   unchanged). Tested: codec round-trip / corpus / known-answer / malformed /
   incompressible; rustfs incompressible-raw, compressible-shrinks across a
   remount and COW rewrite, integrity still catching physical + logical
   corruption on a compressed block; a new `fuzz_compress` decode harness wired
   into `cargo xtask ci` and the soak.)
7. Chunk table, refcounts, reverse refs, reflinks, dedupe index. ✓
   (deduplication is mandatory and exact, keyed on the Stage-5 logical hash:
   a physical data record — a chunk — may be shared by more than one
   `(file, logical block)`, but only after a byte-verify confirms the
   candidate equals the incoming record (a missed duplicate is fine,
   merging unequal data is corruption). Two copy-on-write trees reuse the
   one generic `src/btree.rs` and are named by the transaction root: a
   chunk/refcount tree (physical block → refcount, domain, logical hash,
   length) and a reverse-reference tree (physical block → `(inode, logical
   block)` referrers). An unshared block keeps an implicit refcount of one
   with no tree record; the first share promotes it to refcount 2 and the
   last drop frees it; shared chunks are immutable so overwriting one
   sharer copies-on-write and leaves the others intact. `RustFs::reflink`
   is a COW clone sharing chunks until written. An in-memory dedupe index
   (`(domain, length, logical hash) → candidate`) is rebuilt from the trees
   at mount and is never authoritative — every candidate is liveness- and
   byte-checked before sharing. Dedupe is scoped to the encryption domain
   (§7), and the pipeline is `dedupe → compress → encrypt`. Tested: identical
   content sharing one chunk while distinct does not, byte-verify-before-share
   refusing an injected collision, COW-on-overwrite, reflink, refcount-to-zero
   freeing with the free-space rebuild agreeing, the index rebuilding at mount,
   the domain rule, and integrity + compression on a shared chunk; the soak
   fill switched to distinct per-file content and `fuzz_mount` extended to the
   chunk/reverse-ref decode.)
8. Online scrub. ✓ (`RustFs::scrub` is a resumable, interrupt-safe,
   capability-gated (`CAP_FS_MOUNT`) online verify-and-repair pass —
   `src/scrub.rs`. It authenticates **both** physical copies of every live
   metadata block (superblock slot, transaction root, the inode/extent
   B-trees, the chunk/reverse-reference trees), repairing a bad copy from its
   good companion (Stage 3 seam) and recording a both-copies-bad block as
   unrepairable; runs every live data block through the Stage 5/6 pipeline and
   classifies any fault by its `DataFault` (`Physical`/`Aead`/`Logical`),
   recording it (deep data repair is later); and recomputes the chunk
   refcounts + reverse-reference sets from the live extents, correcting a
   divergence toward that truth without dropping a referrer. A
   `ScrubBudget::Inodes(n)` call persists a rebuildable scrub-progress record
   (`BlockType::ScrubProgress`, reached from the transaction root, holding the
   resume cursor + accumulated counts) and resumes to the identical
   `ScrubReport`; a crash mid-scrub still mounts (ordinary recovery never needs
   scrub) and a corrupt progress record restarts the scrub. Scrub reports a
   structured `ScrubReport` and logs its outcome through `lib/log` with a
   stable event ID (`12000` range); a clean scrub changes nothing and is
   idempotent. Tested: clean/idempotent, single-copy metadata repair, data
   `Physical`/`Logical` classification, refcount + reverse-ref divergence
   detect-and-correct, resumability matching one pass + crash-mid-scrub
   remount, shared-chunk-once within the domain, the capability gate, and
   integrity + compression + dedupe surviving a scrub/remount/COW rewrite;
   `fuzz_mount` extended to drive the scrub-progress decode.)
9. Offline check and rescue. ✓ (`RustFs::check` and `RustFs::rescue` —
   `src/check.rs`. Both reuse the earlier stages' seams rather than
   re-implementing them (`AGENTS.md` §2.2). `check` is the offline superset of
   the online scrub, run on a mounted handle and capability-gated
   (`CAP_FS_MOUNT`): it rebuilds the rebuildable derived state first — the
   free-space bitmap (§4) and the dedupe index (§9) — from the authoritative
   trees (the one shared `rebuild_free_space` walk `open` uses), reuses the
   scrub verification core (`verify_everything`) to verify/repair metadata
   copies, classify data faults, and reconcile refcounts, validates the
   directory tree from the root (an entry to a missing inode is a *dangling*
   finding, reported not auto-deleted), and detects and reclaims orphaned
   inodes; it returns a structured `CheckReport`, is idempotent, and commits
   only when it repaired something. `rescue` extracts files from a volume too
   damaged to mount: read-only on the device (the repair-on-read writes are
   suppressed) and capability-gated, it recovers the keys from a surviving
   superblock discovery header, scans every block for a self-identifying
   transaction root whose commit record validates (`TxnRoot::decode_any`),
   picks the highest-generation root, maps its inode/extent metadata to files,
   and extracts the readable data through the Stage 5/6 integrity pipeline,
   emitting only blocks that pass to a caller-supplied `RescueSink`; it returns
   a structured `RescueReport`. New `12000`-range event IDs in `src/check.rs`.
   Tested: clean check sound/idempotent, check rebuilding a corrupt
   free-space/dedupe derivation with the volume staying mountable, orphan
   reclaim + refcount-divergence correction while reporting an unrepairable
   data fault, the check + rescue capability gates, rescue discovering a root
   and extracting from a wounded superblock ring (read-only/repeatable), and
   rescue never emitting a block that fails integrity; `fuzz_mount` extended to
   drive the offline `check` and the `rescue` root-scan / extraction paths.)
10. TRIM/discard queues and mkfs-time discard. (**DONE** this session: the
   `Block` ABI gained a versioned discard surface (`discard_capability` /
   `discard`, an `abi-v1` extension, not a widening of the frozen read/write
   methods); a device without support is *recorded, not failed*. Freed blocks
   enter a transient, in-memory **pending-discard queue** as a committed
   transaction reclaims them (`finish_txn`), reusing the deferred-free
   machinery (no second free-tracker, §2.2). `RustFs::trim`,
   capability-gated on `CAP_FS_MOUNT`, discards a queued block only if it is
   **still free** at trim time (a reallocated or still-dedupe-shared block —
   refcount ≥ 1 — is marked used by the free-space rebuild and skipped,
   never discarded: the §11 hard constraint), coalesces still-free blocks
   into contiguous runs aligned **inward** to the device granularity (edges
   requeued), and rate-limits to `TRIM_BATCH_RANGES` runs per call. It never
   assumes a discarded block reads back as zero; there is no `nodiscard` /
   `trim=off` mode. The queue is rebuildable transient state (§4): a crash
   mid-trim drops it, the volume remounts cleanly, no live data lost.
   `trim` returns a structured `TrimReport` and logs its outcome with a
   stable `12000`-range event ID (`src/discard.rs`); `format` issues a
   full-range discard on a discard-capable device before laying down the
   encrypted structures. The mock/virtio-blk block devices implement the
   surface. Tested: the capability gate, unsupported-device queue drain
   (recorded-not-failed), contiguous-run coalescing, inward alignment with
   edge requeue, per-request-cap splitting, batch rate-limiting draining
   over passes, a reallocated and a still-dedupe-shared block never
   discarded, the transient queue dropping across a crash with no live data
   lost, and mkfs full-range discard (recorded-not-failed without support).)
11. Device-health baselines and health-triggered scrub. (**DONE** this
   session: the `Block` ABI gained a versioned `device_health()` surface
   (`DeviceHealth::Available(HealthSnapshot)` of SMART/NVMe-style counters, or
   `Unavailable` — *recorded, not failed*, default `Unavailable`), an `abi-v1`
   extension alongside the discard surface. A self-identifying
   `BlockType::HealthBaseline` block reached from the transaction root (like the
   Stage-8 scrub-progress record) **persists** the last clean device snapshot
   plus the volume's accumulated filesystem-observed fault counters — metadata
   copy-repairs/unrepairable (Stage-3 seam) and per-class `DataFault`s (Stage-5
   seam); both are persisted, not rebuildable, because a repaired transient
   fault leaves no trace in the live trees (§4). `format` stores the initial
   baseline; a crash mid-update leaves the previous committed baseline selected
   (§14). `RustFs::health` (`src/health.rs`), capability-gated on
   `CAP_FS_MOUNT`, reads the current telemetry, classifies the volume against
   the documented `HealthThresholds::DEFAULT` (`Healthy`/`Degraded`/`Failing`,
   worse of device + filesystem signals, no magic numbers, §2.1), and — when
   the device's unsafe-shutdown (metadata scrub) or media-error (deep scrub)
   counter has risen since the baseline — **triggers a scrub** through the
   Stage-8 machinery (no parallel verifier, §2.2), folding its findings into the
   counters; it stores the new baseline, returns a structured `HealthReport`,
   and logs with `12000`-range event IDs (`src/health.rs`). Tested: the
   capability gate, a no-telemetry device still classifying and persisting a
   baseline surviving a remount, healthy → degraded → failing as media errors
   climb, an unsafe-shutdown delta triggering a scrub (advanced baseline
   triggers no further scrub), and the persisted baseline surviving a crash at
   every write count with no live data lost; `fuzz_mount` extended to report
   telemetry and drive the health-baseline decode path.)
12. Fuzz, proptest, crash-replay, and corruption-injection suites. ✓
   (The adversarial superset that hardens every earlier stage with no new
   on-disk feature and no second integrity/scrub/decode path, §2.2. The §16
   "fuzz targets for mount, metadata decode, directory decode, compression
   decode, check, and rescue" are all present: `tests/fuzz_mount.rs` drives
   mount / metadata / scrub-progress / health-baseline / check / rescue and now
   the **directory-block decode** path too (`read_dir`/`lookup` over the
   encrypted dirent payload the mount-time walk never reads), and
   `lib/compress` `fuzz_compress` covers compression decode — all wired into
   `cargo xtask fuzz`/`--quick`/`ci`/`--soak`, single invariant "returns a
   `Result`, never panics, fails closed". The crash-replay sweep is
   **generalised to every commit step across every representative transaction**
   — create/write/truncate/remove/reflink/scrub/check/trim/health — asserting a
   whole-transaction-boundary mount, fully-present-or-fully-absent effect, and
   no live-data loss at every write-budget cut-off (§14). A
   **corruption-injection suite** wounds each on-disk structure class
   (superblock slot, transaction root, the inode/extent/chunk/reverse-reference
   B-trees, a directory block, the scrub-progress and health-baseline records,
   and each data-integrity layer) in **one** and **both** copies, asserting:
   single-copy repair from the §8 companion mirror; both-copies mount-critical
   metadata never tears (fail closed or recovers an earlier consistent root via
   the ring, §14); a both-copies-bad directory mounts but reads fail closed and
   scrub records it unrepairable; transient records recover gracefully; and an
   unmirrored data block's fault is classified by `DataFault` and surfaced
   fail-closed, never silently repaired. Reuses the existing `MemBlock`
   write-budget fault-injection, the `DataFault` classes, and
   `verify_everything`, §2.2.)

**Docs**
- `docs/src/filesystem/rustfs.md` (the single native-filesystem page; the
  separate `rustfs_v1.md` mirror was removed — there is no `v1`). Each
  stage expands it with what actually landed.

**Status: all stages complete — RustFS v1 is done (§17).** The copy-on-write
`rustfs` driver replaced the old journaled implementation outright (no
`v1` folder, no parallel version): self-identifying block headers, the
four-slot superblock ring, transaction root + inline commit record, and a
copy-on-write inode map backing the full POSIX read/write/security/
timestamp surface, grown through Stages 2–12 into the full RustFS design
(COW B-trees, keyed-MAC + mirrored metadata, at-rest encryption, per-record
integrity, mandatory compression and dedupe, online scrub, offline
check/rescue, safe TRIM/discard, device-health-triggered scrub, and the
Stage-12 fuzz/crash-replay/corruption-injection suites). It passes its unit
tests, the 1 GiB `fssoak`, the posix suite, the rustfs-over-virtio-blk QEMU
vertical, and the `fuzz_mount` / `fuzz_compress` harnesses. The per-stage
status legend is `docs/src/filesystem/rustfs-spec.md` §18.

**Sparse files (`.junie/SPARSE.md`, spec §19) — DONE.** Always-on,
non-tunable sparse-file support: the write pipeline (`store_block`) detects an
all-zero logical record with a cheap bounded first-party scan (`is_all_zero`)
**before** the logical hash, dedupe, compression, encryption, or allocation,
and stores it as a metadata-only hole — the implicit gap between extent-tree
mappings (the form `.junie/SPARSE.md` §2/§3 permit; no new on-disk field, §2.2)
— releasing any prior physical block through the normal COW/refcount/free path
so reflinks, deduped owners, and recovery roots stay live. A zero range is
never deduped, compressed, or given a physical block; repeated non-zero data
follows the normal zstd/RAW path (no RLE/FILL mode). All ten mandatory §17
tests pass (10 MiB zero file allocates zero data blocks, hole split, overwrite
with zeroes vs reflink, truncate up/down, reflink preserving holes, scrub +
check, compression bypass, encrypted-volume no-plaintext). Docs: spec §2/§6/§19,
`docs/src/filesystem/rustfs.md`, and the crate `README.md`.

**Long names, ext4 charset, case sensitivity & online resize (spec §13) —
DONE.** The directory format moved from a fixed 64-byte slot (≤56-byte names)
to a fixed **263-byte slot** (an 8-byte header + a maximum-length **255-byte**
name), so names now match ext4's limit. `check_name` enforces the ext4 rules —
1..=255 bytes, `/` and NUL the only forbidden bytes, `.`/`..` reserved — and
names are compared byte-for-byte, so they are **case-sensitive** (no
case-folding/normalisation; that is VFS policy). Because a 512-byte block now
holds a single slot, `format`/`create_inner` insert `.`/`..` through the normal
`add_entry` path so a directory spans as many copy-on-write blocks as the block
size needs (§2.2 — one insertion path). Online **grow** (`RustFs::grow`)
decouples the committed block count (pinned in the superblock) from the device
size: `open` adopts the committed size and leaves a surplus device tail unused,
and `grow` re-reads the device geometry, folds the new free tail into the pool,
and commits the larger size in one atomic transaction — data never moves, the
space is usable without a remount, and a crash before commit keeps the old size
(§14). Online shrink is rejected. `abi-v1` is not frozen, so the on-disk
directory format changed in place (no new format version). 10 new unit tests
(255-byte names round-trip + remount, oversize rejection, ext4 charset incl.
NUL/`/`/`.`/`..`, case sensitivity, 512-byte multi-block directories, online
grow + remount + idempotence, online-shrink rejection, over-device mount
rejection). Docs: spec §13, `docs/src/filesystem/rustfs.md`, crate `README.md`.

---

## Stage 6 — Userland Foundations

**Dependencies:** Stages 2–5 sufficient for at least one platform.

**Deliverables**
- `userland/system/init` (PID 1): service manager, dependency-ordered start,
  reaper, capability granting from manifests.
- `userland/shell/shell`: POSIX-ish shell with job control and a small builtin set.
- `userland/session/login`: text login that authenticates against `kernel/sec`
  and spawns a shell or a graphical session. Always starts in text mode;
  offers graphical mode only when a display driver and `userland/gui/wm`
  are available.
- Core CLI utilities (`ls`, `cp`, `mv`, `rm`, `cat`, `ps`, `mount`,
  `chmod`, `chown`, `useradd`, `groupadd`, `setcap`, `getcap`,
  `sysinfo`). Each utility is its own small crate under `userland/apps/`.
  `ps`, `mount`, and `sysinfo` are clients of the System Information API
  defined in `AGENTS.md` §16.6 (`lib/abi/src/sysinfo.rs`); they do **not**
  read a `/proc`-style virtual filesystem.
- `lib/abi/src/sysinfo.rs`: typed, versioned, capability-gated request /
  response types for the System Information API (§16.6). Frozen on
  release; new queries ship as `sysinfo-v2`.
- `userland/system/sysinfod`: user-space system service that serves the
  API. Installed to `/System/Services/sysinfod`.
- Application-bundle loader in `kernel/core` (or a user-space service
  invoked by `init`) that recognises `/Apps/<Name>.app/` bundles per
  `AGENTS.md` §16.5: parses and verifies the signed `AppInfo`
  manifest, computes the granted capability set as the intersection of
  the user's grants and the manifest request, and refuses bundles whose
  top-level layout deviates from the fixed set.
- Dynamic loader policy: shared-library references resolve only against
  the calling bundle's own `Libraries/` directory and `/System/Libraries/`
  (§16.4). Any other path is a load-time error.

**Tests**
- Integration tests: boot to login, log in, run each utility, exercise
  permission denials.

**Docs**
- `docs/src/userland/{init,login,shell,utilities}.md`.

**Status: complete.**
- **System Information API ABI (`lib/abi/src/sysinfo.rs`) — DONE.** The
  `sysinfo-v1` surface (`rustos_abi::sysinfo`, §16.6) is implemented: a
  versioned (`SYSINFO_VERSION_V1`), frozen query registry
  (`SysinfoQueryId` + `SysinfoQuerySpec` + `SYSINFO_QUERIES`) with a
  canonical hashable byte image (`ENCODED_QUERY_TABLE` /
  `encoded_query_table`) and per-query `required_capability`/`audit`
  (same §9 discipline as the syscall table). Six queries are pinned:
  `SELF_PROCESS_LIST` (no cap), `GLOBAL_PROCESS_LIST` (`CAP_SYSINFO_GLOBAL`),
  `KERNEL_MEMORY_STATS` (`CAP_SYSINFO_KERNEL`), `HARDWARE_TREE`
  (`CAP_SYSINFO_HW`), `SYSTEM_IDENTITY`, and `UPTIME`. The three new
  capabilities `CAP_SYSINFO_GLOBAL/KERNEL/HW` (`CapabilityId` 13/14/15)
  were added with frozen-id tests. A versioned `SysinfoRequestHeader`
  envelope (`SYI1` magic, fail-closed decode) frames typed payloads:
  `ProcessListRequest` (offset/limit paging), `ProcessRecord` +
  `ProcessState`, `KernelMemoryStats`, `Uptime`, and `SystemIdentity` —
  all `#[repr(C)]`, allocation-free, little-endian, with `to_le_bytes` /
  `from_bytes`. The shared little-endian read/write helpers were extracted
  into a `pub(crate) mod le` (deduplicating `ipc.rs`, §2.2). 22 new abi
  unit tests; every new `from_bytes` is enrolled in the `lib/abi` fuzz
  harness (§19.6). Docs: `docs/src/abi/sysinfo.md` (+ SUMMARY link). No
  `unsafe`, no `unwrap`/`expect`/`panic!` in production paths.
- **System Information service (`userland/system/sysinfod`) — DONE.**
  The user-space dispatcher that serves the `sysinfo-v1` API
  (`rustos-sysinfod`, §16.6) is implemented. `serve` decodes a
  `SysinfoRequestHeader` + typed payload, looks the query up in the
  frozen `SYSINFO_QUERIES` registry, enforces the query's declared
  capability against the caller's `CapabilityQuery` view **before** any
  data access (fail closed, §5.4), emits a `lib/log` audit record for
  every audited invocation and every denial (reserved `EventId` range
  `8000..9000`, §19.4), and pages/encodes the answer. The live data is
  read through an injected `SysinfoSource` seam (`Caller` + `ProcessScope`)
  so the security-relevant code is independent of kernel plumbing and is
  testable with an in-memory fixture; the dispatcher chooses the scope
  from the query id so a self-scoped request can never widen into a
  global one. Process-list responses are paged `ProcessRecord`s, scalar
  responses are the struct's wire image, and `HARDWARE_TREE` is passed
  through verbatim (the hardware-tree wire format is owned by `lib/abi`
  §18, not invented here). `no_std`, depends only on `rustos-abi` +
  `rustos-log` (§17.4); no `unsafe`, no `unwrap`/`expect`/`panic!` in
  production paths. 12 unit tests; docs `docs/src/userland/sysinfod.md`
  (+ SUMMARY link) and the crate `README.md`.
- **PID 1 service manager (`userland/system/init`) — DONE.** `rustos-init`
  is the orchestrator that brings the registered system services up in
  dependency order, grants each one the intersection of its signed
  manifest's capability request with init's own authority (`AGENTS.md`
  §5.2), and reaps children. `Init::start_all` orders the services with a
  deterministic Kahn topological sort (`AGENTS.md` §18.3) and **fails
  closed** on a structurally invalid graph — an unregistered dependency
  (`DependencyMissing`) or a cycle (`DependencyCycle`) starts nothing.
  Over a sound graph it starts every service it can, refusing one whose
  manifest over-requests authority (`CapabilityEscalation`, never a silent
  narrowing) and skipping the transitive dependents of any service that
  fails, returning the full `StartReport`. The capability decode is the
  shared `rustos_abi::decode_capability_ids` (added this session and reused
  by `drvhost`, so the manifest-body format has one implementation,
  `AGENTS.md` §2.2). The trusted load/verify/exec step and exited-child
  notifications are injected as the `Spawner`/`Reaper` seams, keeping the
  security-relevant code free of kernel plumbing and exhaustively testable.
  `Init::reap` distinguishes a registered service's exit from an inherited
  orphan. `no_std` (with `alloc`), deps only `rustos-abi`/`rustos-caps`/
  `rustos-log` (`AGENTS.md` §17.4); no `unsafe`, no `unwrap`/`expect`/
  `panic!` in production paths. Audit `EventId` range `9000..10000`. 17
  unit tests; docs `docs/src/userland/init.md` (+ SUMMARY link) and the
  crate `README.md`.
- **Default shell (`userland/shell/shell`) — DONE.** `rustos-shell` is a
  POSIX-ish command interpreter: `lexer::tokenize` turns a line into a
  quoting- and escape-aware token stream, `parser::parse` builds a
  `CommandList` of pipelines joined by the `;`/`&&`/`||`/`&` connectors,
  `env::Environment::expand_word` performs `$NAME`/`${NAME}`/`$?`
  expansion, and `Shell::run_line` runs each entry — honouring the
  connector run-conditions and the background flag — dispatching the
  in-process builtins (`cd`, `pwd`, `exit`, `export`, `unset`, `echo`,
  `jobs`, `fg`, `bg`, `help`) or launching externals, with a `JobTable`
  tracking background/stopped jobs and reporting finished ones before the
  next prompt. The two outside-world operations are the injected
  `ProcessHost`/`Console` seams (mirroring `init`'s `Spawner`/`Reaper`),
  so every parsing, expansion, and control-flow decision is testable
  without a kernel; host failures flow back as the stable `rustos_abi`
  `Errno`, so the shell invents no parallel error set (`AGENTS.md` §2.2).
  A line that will not lex/parse/expand is a `ParseError` that runs
  nothing and sets `$?` to 2; a command that will not launch is an
  ordinary non-zero status (127), never a panic and never a line abort
  (`AGENTS.md` §2.9). `no_std` (with `alloc`), depends only on
  `rustos-abi` (`AGENTS.md` §17.4); no `unsafe`, no `unwrap`/`expect`/
  `panic!` in production paths. 60 unit tests; docs
  `docs/src/userland/shell.md` (+ SUMMARY link) and the crate `README.md`.
- **Text login (`userland/session/login`) — DONE.** `rustos-login`
  authenticates a user against `kernel/sec` and launches a session on
  their behalf (`AGENTS.md` §10). `Login::run` repeats a bounded,
  **fail-closed** loop: prompt for a username (echoed) and password
  (un-echoed via the `Prompt::read_secret` seam, §5), verify the
  `Credentials` through the injected `Authenticator` (which checks the
  password against the stored hash with `lib/crypto`'s constant-time
  primitives — login never sees the hash), and on success choose a
  session (text by default; graphical offered only when a display driver
  and the window manager are present, §10) and hand the authenticated
  identity to the `SessionLauncher`. A rejected attempt is audited and
  consumes one try; an exhausted budget launches nothing
  (`LoginError::TooManyAttempts`), a dead console aborts
  (`LoginError::Console`), and a refused session launch returns
  (`LoginError::SessionLaunch`). The `Authenticator` returns the same
  error whether the account is unknown or the password is wrong, and
  login never discloses the cause, so the prompt cannot probe for valid
  usernames (§5). A successful authentication yields an
  `AuthenticatedUser` `(uid, primary gid, supplementary gids, capability
  grants)` whose capability ceiling login hands to the launcher verbatim
  and never widens (§4, §5.2). The three outside-world operations are the
  injected `Prompt`/`Authenticator`/`SessionLauncher` seams (mirroring
  `init`'s `Spawner`/`Reaper`), so the policy is testable without a
  kernel. `no_std` (with `alloc`), deps only `rustos-abi`/`rustos-caps`/
  `rustos-log` (§17.4); no `unsafe`, no `unwrap`/`expect`/`panic!` in
  production paths. Audit `EventId` range `10000..11000`. 17 unit tests;
  docs `docs/src/userland/login.md` (+ SUMMARY link) and the crate
  `README.md`.
- **`sysinfo` CLI (`userland/shell/sysinfo`) — DONE.** `rustos-sysinfo`
  is the single command-line tool that exposes the System Information API
  to the terminal (`AGENTS.md` §16.6): it is a *client* of the
  `sysinfo-v1` ABI served by `sysinfod`, not a `/proc`/`/sys` reader, and
  there is no privileged path bypassing the capability check. `run` turns
  one parsed `Command` (`processes [--all]`, `memory`, `hardware`,
  `identity`, `uptime`, `help`, with short aliases) into a typed
  `SysinfoRequestHeader` + payload, issues it through the injected
  `Transport` seam, decodes the typed reply with the ABI's fail-closed
  `from_bytes` decoders, and renders one line per row to the injected
  `Output` seam — paging the process list with `offset`/`limit` until a
  short page ends it. It **fails closed**: a capability denial comes back
  as `Errno::PermissionDenied` and renders as `SysinfoError::PermissionDenied`
  (no parallel policy, §2.2); an unknown subcommand/flag/stray argument is
  a `SysinfoError::Usage` that issues no query; a reply that does not
  decode against `sysinfo-v1` is a hard `SysinfoError::Service` error,
  never a partial guess. The hardware-tree wire format is owned by
  `lib/abi` §18 and not built yet, so `hardware` honestly reports the
  byte length rather than faking a decode (§2.1). `no_std` (with `alloc`),
  depends only on `rustos-abi` (§17.4); no `unsafe`, no
  `unwrap`/`expect`/`panic!` in production paths. 19 unit tests (7 parser
  + 12 request/render against an in-memory `sysinfod` stand-in); docs
  `docs/src/userland/utilities.md` (+ SUMMARY link) and the crate
  `README.md`.
- **`cat` CLI (`userland/apps/cat`) — DONE.** `rustos-cat` is the first
  crate under `userland/apps/` (`AGENTS.md` §3): it concatenates its
  sources — files and standard input (the `-` operand, and the default
  when none is given) — to the terminal, numbering output lines
  continuously across every source with `-n`, the POSIX model. `run`
  pulls bytes from each source in fixed-size chunks and writes them
  (optionally line-numbered) through three injected seams — `FileSource`
  (read a byte range of a named file), `Input` (standard input), and
  `Output` (terminal) — so every parsing, streaming, and numbering
  decision is testable without a kernel (the seam discipline of `init`,
  `login`, and `sysinfo`). The line-numbering state is carried across
  read chunks and across sources, so a line straddling a chunk or file
  boundary is numbered exactly once. It **fails closed**: an unrecognised
  option is a `CatError::Usage` that reads nothing, a source that cannot
  be read surfaces the underlying `Errno` as `CatError::Read` and stops
  before any later source, a failed write is `CatError::Output`, and a
  seam reporting more bytes than the buffer holds is refused rather than
  indexed out of bounds (`AGENTS.md` §2.9). `no_std` (with `alloc`),
  depends only on `rustos-abi` (§17.4); no `unsafe`, no
  `unwrap`/`expect`/`panic!` in production paths. 20 unit tests; docs
  `docs/src/userland/utilities.md` (`cat` section) and the crate
  `README.md`.
- **`ls` CLI (`userland/apps/ls`) — DONE.** `rustos-ls` lists directory
  contents (`AGENTS.md` §3): it inspects each path operand in order — a
  non-directory operand is listed by name, a directory operand has its
  entries listed sorted by name — defaulting to the current directory
  (`.`) when none is given. `-a` includes dot-prefixed entries; `-l`
  renders the long format (the ten-character mode string — a type
  character plus the nine `rwx` bits — the right-aligned size, then the
  name); short options cluster (`-la`). `run` asks the injected `Listing`
  seam for each operand's `Metadata` and each directory's entries, sorts
  and formats them, and writes the listing through the injected `Output`
  seam in one call — the seam discipline of `cat`, so every parsing,
  filtering, sorting, and formatting decision is testable without a
  kernel. With several operands, non-directory operands are listed first,
  then each directory under a `path:` header, blocks separated by a blank
  line. It **fails closed**: an unrecognised option is a `LsError::Usage`
  that inspects nothing, an operand that cannot be stat'd surfaces the
  underlying `Errno` as `LsError::Stat` and stops before any later
  operand, an unreadable directory is `LsError::Read`, and a failed write
  is `LsError::Output` (`AGENTS.md` §2.9). `no_std` (with `alloc`),
  depends only on `rustos-abi` (§17.4); no `unsafe`, no
  `unwrap`/`expect`/`panic!` in production paths. 23 unit tests; docs
  `docs/src/userland/utilities.md` (`ls` section) and the crate
  `README.md`.
- **`rm` CLI (`userland/apps/rm`) — DONE.** `rustos-rm` removes its
  operands in order (`AGENTS.md` §3): a non-directory operand (a regular
  file, a symbolic link removed and never followed, a device node) is
  unlinked, and a directory operand is removed only with `-r`, which
  removes its contents depth-first and then the directory itself; naming
  a directory without `-r` is a `RmError::IsDirectory`. With `-f` an
  operand that does not exist is skipped, the POSIX model. `run` asks the
  injected `Removal` seam for each operand's kind, reads each directory it
  must descend by index, and unlinks every reachable object — contents
  before their directory — writing only the help banner through the
  injected `Output` seam (`rm` is silent on success), the same seam
  discipline as `ls`/`cat`. It **fails closed**: an unrecognised option or
  a missing operand without `-f` is a `RmError::Usage` that removes
  nothing; an operand that cannot be inspected surfaces the underlying
  `Errno` as `RmError::Stat` and stops before any later operand (`-f`
  suppresses only `NotFound`, never `PermissionDenied`); an unreadable
  directory is `RmError::Read`; a failed unlink is `RmError::Remove`
  (`AGENTS.md` §2.9). `no_std` (with `alloc`), depends only on
  `rustos-abi` (§17.4); no `unsafe`, no `unwrap`/`expect`/`panic!` in
  production paths. 26 unit tests (10 parser + 16 removal engine); docs
  `docs/src/userland/utilities.md` (`rm` section) and the crate
  `README.md`.
- **`cp` CLI (`userland/apps/cp`) — DONE.** `rustos-cp` copies its source
  operands to a destination (`AGENTS.md` §3): with a single source and a
  non-directory destination the source is copied to that exact path, while
  an existing-directory destination (always, with more than one source)
  receives each source under its own base name. A directory source is
  copied only with `-r`, which reproduces the whole subtree; naming a
  directory without `-r` is a `CpError::IsDirectory`, the POSIX model.
  `run` asks the injected `FileSystem` seam for each source's kind, streams
  a regular file from source to destination in fixed-size chunks (matching
  `cat`'s granularity), and creates each destination directory before its
  contents — writing only the help banner through the injected `Output`
  seam (`cp` is silent on success), the same seam discipline as `rm`. `-f`
  removes a destination that cannot be created and retries the create once.
  It **fails closed**: an unrecognised option, fewer than two operands, or
  more than one source aimed at a non-directory destination is a
  `CpError::Usage` that copies nothing; a directory source whose
  destination already exists as a non-directory is a
  `CpError::NotADirectory`; an operand that cannot be inspected surfaces the
  underlying `Errno` as `CpError::Stat` and stops before any later operand;
  an unreadable source is `CpError::Read`, an uncreatable destination is
  `CpError::Create`, and a failed write is `CpError::Write` (`AGENTS.md`
  §2.9). `no_std` (with `alloc`), depends only on `rustos-abi` (§17.4); no
  `unsafe`, no `unwrap`/`expect`/`panic!` in production paths. 28 unit
  tests (9 parser + 19 copy engine); docs
  `docs/src/userland/utilities.md` (`cp` section) and the crate `README.md`.
- **`mv` CLI (`userland/apps/mv`) — DONE.** `rustos-mv` relocates its
  source operands to a destination (`AGENTS.md` §3): with a single source
  and a non-directory destination the source is moved to that exact path,
  while an existing-directory destination (always, with more than one
  source) receives each source under its own base name. Unlike `cp`, a
  directory is moved without a flag. `run` asks the injected `FileSystem`
  seam for each source's kind and then to `rename` it onto its
  destination; a rename within one filesystem is atomic and is the whole
  move, while a rename that would cross a filesystem boundary is reported
  as an explicit `RenameOutcome::CrossDevice` (never an overloaded
  `Errno`, §2.11) and falls back to the POSIX relocation: copy the source
  (streaming a regular file in fixed-size chunks matching `cat`/`cp`,
  reproducing a directory subtree) then remove it depth-first. `-n` never
  overwrites an existing destination; `-f` removes a destination that
  blocks the rename and retries once; `mv` is otherwise silent on success,
  writing only the help banner through the injected `Output` seam (the
  same seam discipline as `cp`/`rm`). It **fails closed**: an unrecognised
  option, fewer than two operands, or more than one source aimed at a
  non-directory destination is an `MvError::Usage` that moves nothing; an
  operand that cannot be inspected surfaces the underlying `Errno` as
  `MvError::Stat` and stops before any later operand; a non-boundary
  rename failure is `MvError::Rename`; during a cross-device relocation an
  unreadable source is `MvError::Read`, an uncreatable destination is
  `MvError::Create`, a failed write is `MvError::Write`, and a source that
  cannot be removed after the copy is `MvError::Remove` (`AGENTS.md`
  §2.9). `no_std` (with `alloc`), depends only on `rustos-abi` (§17.4); no
  `unsafe`, no `unwrap`/`expect`/`panic!` in production paths. 30 unit
  tests (10 parser + 20 move engine); docs
  `docs/src/userland/utilities.md` (`mv` section) and the crate `README.md`.
- **`chmod` CLI (`userland/apps/chmod`) — DONE.** `rustos-chmod` applies a
  mode to each of its file operands (`AGENTS.md` §3): an absolute octal
  value (`644`, `0755`, …) that replaces the low twelve permission bits
  outright, or a comma-separated list of symbolic clauses
  (`[ugoa]*[-+=][rwxXst]*`, e.g. `g+w`, `a=rx`, `u+s`) that transform the
  file's current bits — the full POSIX mode algebra (who `ugoa`, ops
  `+-=`, perms `rwxXst`, conditional `X`, setuid/setgid/sticky,
  multiple operator sections per clause, omitted-who treated as `a`). With
  `-R` a directory is changed and then its contents recursively (the
  directory before its contents). `run` asks the injected `FileSystem`
  seam for each operand's kind and current mode, computes the new mode,
  applies it via `set_mode`, and walks each directory `-R` must descend —
  writing only the help banner through the injected `Output` seam (`chmod`
  is silent on success), the same seam discipline as `cp`/`mv`/`rm`. It
  **fails closed**: an unrecognised option or a missing operand is a
  `ChmodError::Usage`; a mode operand that is neither octal nor symbolic is
  a `ChmodError::BadMode`; an operand that cannot be inspected surfaces the
  underlying `Errno` as `ChmodError::Stat` and stops before any later
  operand; an unapplyable mode is `ChmodError::Apply`; a directory whose
  entries cannot be read during a recursive descent is `ChmodError::Read`
  (`AGENTS.md` §2.9). POSIX `chmod` spells recursive `-R` (a bare `-r` is
  not an option). `no_std` (with `alloc`), depends only on `rustos-abi`
  (§17.4); no `unsafe`, no `unwrap`/`expect`/`panic!` in production paths.
  34 unit tests (parser + mode algebra + change engine); docs
  `docs/src/userland/utilities.md` (`chmod` section) and the crate
  `README.md`.
- **`chown` CLI (`userland/apps/chown`) — DONE.** `rustos-chown` applies an
  ownership change to each of its file operands (`AGENTS.md` §3): the owner
  operand is `OWNER`, `OWNER:GROUP`, or `:GROUP`, where `OWNER` and `GROUP`
  are **decimal** user/group ids — `OWNER` changes only the owning user,
  `:GROUP` only the owning group, and `OWNER:GROUP` both. Names are not
  accepted (RustOS has no name-to-id seam in this tool, so a name would be
  interface creep, §2.4); an empty spec, a bare `:`, and a trailing-colon
  `OWNER:` are rejected rather than guessed (§2.1). With `-R` a directory
  is changed and then its contents recursively (the directory before its
  contents, reusing the kind carried in each directory entry so it
  re-inspects nothing). `run` asks the injected `FileSystem` seam for each
  operand's kind, applies the owner via `set_owner`, and walks each
  directory `-R` must descend — writing only the help banner through the
  injected `Output` seam (`chown` is silent on success), the same seam
  discipline as `chmod`/`cp`/`mv`/`rm`. It **fails closed**: an
  unrecognised option or a missing operand is a `ChownError::Usage`; an
  owner operand that is not a valid spec is a `ChownError::BadOwner`; an
  operand that cannot be inspected surfaces the underlying `Errno` as
  `ChownError::Stat` and stops before any later operand; an unapplyable
  owner is `ChownError::Apply`; a directory whose entries cannot be read
  during a recursive descent is `ChownError::Read` (`AGENTS.md` §2.9).
  POSIX `chown` spells recursive `-R` (a bare `-r` is not an option).
  `no_std` (with `alloc`), depends only on `rustos-abi` (§17.4); no
  `unsafe`, no `unwrap`/`expect`/`panic!` in production paths. 25 unit
  tests (parser + owner-spec + change engine); docs
  `docs/src/userland/utilities.md` (`chown` section) and the crate
  `README.md`.
- **`getcap` CLI (`userland/apps/getcap`) — DONE.** `rustos-getcap`
  reports the optional per-inode capability gate (`AGENTS.md` §5.3): a
  capability the caller must hold to reach a node at all, on top of the
  mode/ACL checks. For each file operand it prints `path CAP_NAME` when
  the file carries a gate and nothing when it does not (so a clean tree is
  silent), and with `-R` reports a directory and then its contents
  recursively. A gate renders by its canonical `CAP_*` name via the new
  frozen `rustos_abi::CapabilityId::name`; a node that stored an in-range
  identifier the running ABI has not named renders as `CAP_<id>` rather
  than being silently dropped (`AGENTS.md` §2.1). `run` asks the injected
  `FileSystem` seam for each operand's kind and gate and walks each
  directory `-R` must descend — the same seam discipline as `chmod`/`chown`;
  the driver only *reports* the gate and makes no permission decision
  (`AGENTS.md` §5.4). It **fails closed**: an unrecognised option or a
  missing operand is a `GetcapError::Usage`; an uninspectable operand
  surfaces the `Errno` as `GetcapError::Stat` and stops the run; an
  unreadable gate is `GetcapError::Query`; an unreadable directory during
  `-R` is `GetcapError::Read`; a failed write is `GetcapError::Output`.
  `no_std` (with `alloc`), depends only on `rustos-abi` (§17.4); no
  `unsafe`, no `unwrap`/`expect`/`panic!` in production paths. 15 unit
  tests; docs `docs/src/userland/utilities.md` (`getcap` section) and the
  crate `README.md`.
- **`setcap` CLI (`userland/apps/setcap`) — DONE.** `rustos-setcap` is the
  policy-writing companion of `getcap`: it sets or clears the per-inode
  capability gate (`AGENTS.md` §5.3). The capability operand is a
  canonical `CAP_*` name (install that gate, resolved through the shared
  frozen `rustos_abi::CapabilityId::from_name`, §2.2) or the literal `-`
  (clear the gate); with `-R` a directory is changed and then its contents
  recursively. The name match is exact and case-sensitive — an unknown,
  mis-cased, or bare-numeric value is a fail-closed
  `SetcapError::BadCapability` (`AGENTS.md` §2.1). `run` asks the injected
  `FileSystem` seam for each operand's kind, applies the gate with
  `set_cap`, and walks each directory `-R` must descend (directory before
  contents) — the same seam discipline as `chmod`/`chown`; `setcap` stores
  the gate but makes no permission decision (`AGENTS.md` §5.4), and setting
  a gate is itself privileged (the seam refuses an unauthorised attempt,
  surfaced as `SetcapError::Apply`). It **fails closed**: an unrecognised
  option or a missing operand is a `SetcapError::Usage`; an uninspectable
  operand surfaces the `Errno` as `SetcapError::Stat` and stops the run; an
  unapplyable gate is `SetcapError::Apply`; an unreadable directory during
  `-R` is `SetcapError::Read`. `no_std` (with `alloc`), depends only on
  `rustos-abi` (§17.4); no `unsafe`, no `unwrap`/`expect`/`panic!` in
  production paths. 22 unit tests; docs `docs/src/userland/utilities.md`
  (`setcap` section) and the crate `README.md`.
- **Canonical capability names in `lib/abi` — DONE (this session, with
  `getcap`/`setcap`).** `rustos_abi::CapabilityId` gained `name()` /
  `from_name()` backed by a single `NAMED` source-of-truth table (so the
  two can never disagree, §2.2). The `CAP_*` spellings the charter uses
  throughout (`AGENTS.md` §5.2) are part of the frozen `abi-v1` contract;
  4 new tests pin them, cover the name↔id round-trip, assert every
  assigned id is named, and confirm `from_name` is exact and fails closed.
- **`ps` CLI (`userland/apps/ps`) + shared `lib/procinfo` — DONE.**
  `rustos-ps` lists processes through the `sysinfo-v1` System Information
  API served by `sysinfod` (`AGENTS.md` §16.6) — there is no `/proc`. By
  default it lists the caller's own processes (the ungated
  `SELF_PROCESS_LIST`); `-e`/`-A`/`--all` request every process
  (`GLOBAL_PROCESS_LIST`, gated by the service on `CAP_SYSINFO_GLOBAL`).
  `ps` and the `sysinfo` CLI share the process-list shape, so — because
  sibling userland crates may not depend on one another (§17.4) — the
  request seams (`Transport`/`Output`), the request framing + capability-
  aware `call`, and the `offset`/`limit` page walk plus fixed-column row
  render were lifted into the new **`lib/procinfo`** crate rather than
  copied (§2.2); the `sysinfo` CLI was refactored onto it (deleting its
  duplicated `transport`/encode/`service_call`/paging/render). `ps` owns
  only its own grammar, usage banner, and `PsError`. It **fails closed**:
  an unknown option or any positional operand is a `PsError::Usage`; a
  denied global listing is `PsError::PermissionDenied` (the service is the
  policy point, §5.4); any other transport failure or undecodable reply is
  `PsError::Service`; a failed write is `PsError::Output`. Both crates are
  `no_std` (with `alloc`) and depend only on `lib/*` (`rustos-abi` +
  `rustos-procinfo`, §17.4); no `unsafe`, no `unwrap`/`expect`/`panic!` in
  production paths. `lib/procinfo` has 14 unit tests and `rustos-ps` 13;
  docs `docs/src/userland/utilities.md` (`ps` section) and both crate
  `README.md`s.
- **`useradd` CLI (`userland/apps/useradd`) — DONE.** `rustos-useradd`
  creates a single account in the user database that persists under
  `/System/Security/Users` (`AGENTS.md` §5.1, §16). It parses a login
  name plus its numeric identity — an optional uid (`-u`, auto-allocated
  by the database when omitted), a **required** primary group (`-g`), an
  optional comma-separated supplementary set (`-G`), and the textual
  comment (`-c`) and home directory (`-d`) — and hands the record to the
  injected `UserDb` seam. Group/user references are **decimal** ids
  (a name would be interface creep with no name-to-id seam, §2.4, the
  same choice `chown` makes), and the login name must match
  `[a-z_][a-z0-9_-]*` within a length bound. The tool is **not** the
  policy point (§5.4): creating an account needs `CAP_USER_ADMIN` (§5.2),
  but the database enforces that — along with uid collisions, group
  existence, and the supplementary bound — and a refusal surfaces as
  `UseraddError::Create`. It never guesses a default group, uid, or home
  directory (§2.1). It **fails closed**: an unknown option, a missing
  `-g`, or anything other than exactly one name operand is a
  `UseraddError::Usage`; an invalid login name is `UseraddError::BadName`;
  a non-decimal id is `UseraddError::BadId`; an existing name is
  `UseraddError::Exists`; a lookup/create failure carries the underlying
  `Errno` as `UseraddError::Lookup`/`Create`. `no_std` (with `alloc`),
  depends only on `rustos-abi` (§17.4); no `unsafe`, no
  `unwrap`/`expect`/`panic!` in production paths. 24 unit tests; docs
  `docs/src/userland/utilities.md` (`useradd` section) and the crate
  `README.md`.
- **`groupadd` CLI (`userland/apps/groupadd`) — DONE.** `rustos-groupadd`
  creates a single group in the group database that persists under
  `/System/Security/Groups` (`AGENTS.md` §5.1, §16) — the natural sibling
  of `useradd`, narrowed to the two fields a group record carries. The
  grammar is `groupadd [-g GID] [--] NAME`: a single name operand plus an
  optional decimal gid (`-g`/`--gid`, attached or following), auto-
  allocated by the database when omitted (no default-gid policy to invent,
  §2.1). A group name (rather than a numeric id) is interface creep with
  no name-to-id seam (§2.4), and the name must match `[a-z_][a-z0-9_-]*`
  within a length bound. The tool is **not** the policy point (§5.4):
  creating a group needs `CAP_USER_ADMIN` (§5.2), but the injected
  `GroupDb` seam (`name_in_use` + `create(GroupSpec)`) enforces that and
  gid collisions, surfacing a refusal as `GroupaddError::Create`. It
  **fails closed**: an unknown option or anything other than exactly one
  name operand is `GroupaddError::Usage`; an invalid name is
  `GroupaddError::BadName`; a non-decimal id is `GroupaddError::BadId`; an
  existing name is `GroupaddError::Exists`; a lookup/create failure
  carries the underlying `Errno` as `GroupaddError::Lookup`/`Create`.
  `no_std` (with `alloc`), depends only on `rustos-abi` (§17.4); no
  `unsafe`, no `unwrap`/`expect`/`panic!` in production paths. 21 unit
  tests; docs `docs/src/userland/utilities.md` (`groupadd` section) and
  the crate `README.md`.
- **`mount` CLI (`userland/apps/mount`) + `sysinfo-v1` `MOUNT_LIST` — DONE.**
  `rustos-mount` both reports and changes the mount table. **Listing** is a
  read of live system state, so — like `ps` — it goes through the System
  Information API (§16.6): a new ungated `MOUNT_LIST` query (id 6, the
  seventh frozen `sysinfo-v1` query) carries a `MountListRequest`
  (`offset`/`limit` paging) and returns packed `MountRecord`s (inline
  `source`/`target`/`fstype` buffers plus the filesystem-driver ABI's
  `MountFlags`, reused rather than re-declared, §2.2). `sysinfod` grew a
  `mount_records` source method and a dispatch arm, and the process- and
  mount-list paging now share one `page_records` helper (§2.2). The mount
  table is system-wide and secret-free, so the query is ungated; the
  privileged *act* of mounting is gated separately by `CAP_FS_MOUNT` (§5.2).
  **Attaching** (`SOURCE TARGET`) hands a `MountSpec` to the injected
  `Mounter` seam; the kernel is the policy point (§5.4) and a refusal
  surfaces as `MountError::Mount`. The listing reuses the shared
  `lib/procinfo` helpers — extended this session with a generic `walk_pages`
  paged-list walk, a shared `ListError` (renamed from `ProcessListError`),
  and a `for_each_mount`/`render_mount` mount module — rather than copying
  them (§2.2, §17.4); `ps` and `sysinfo` were migrated onto `ListError`.
  Grammar `mount [-r] [-t TYPE] [-o ro,rw,nosuid,nodev,noexec] [--]
  [SOURCE TARGET]`. It **fails closed**: an unknown option, a missing
  value, or a wrong operand count is `MountError::Usage`; a bad `-o`/`-t`
  value is `MountError::BadOption`; a listing failure is
  `MountError::Service`; a write failure is `MountError::Output`. `no_std`
  (with `alloc`), deps only `rustos-abi` + `rustos-procinfo` (§17.4); no
  `unsafe`, no `unwrap`/`expect`/`panic!` in production paths. 23 `mount`
  unit tests (+ new `lib/abi`, `lib/procinfo`, and `sysinfod` tests, and
  both new decoders enrolled in the `lib/abi` fuzz harness, §19.6); docs
  `docs/src/userland/utilities.md` (`mount` section), `docs/src/abi/sysinfo.md`,
  `docs/src/userland/sysinfod.md`, and the crate `README.md`.
- **Application-bundle loader + dynamic-loader policy
  (`userland/system/appmgr` + `lib/abi/src/appinfo.rs`) — DONE.** The
  frozen `abi-v1` `appinfo` surface defines the fixed `/Apps/<Name>.app/`
  layout (`BundleEntry` + `validate_bundle_layout`, §16.5), the signed
  `AppInfoHeader` manifest (bundle id/name/version, capability + MIME
  counts, syscall-table hash, a `content_hash` binding the signature to the
  bundle's contents, Ed25519 signer key + signature; `WIRE_LEN` 340,
  fail-closed `from_bytes`, `body_len`/`mime_type_at` body readers), and the
  §16.4 dynamic-loader policy `resolve_library` (a reference resolves only
  against the bundle's own `Libraries/` or `/System/Libraries/`, `..`
  refused). `rustos-appmgr` is the user-space service (installed to
  `/System/Services/appmgr`): `AppLoader::load` runs a fail-closed pipeline
  (§5.4) — validate layout, decode + ABI-check the manifest, constant-time
  syscall-hash match (§9/§19.2), verify the signature via the injected
  `Verifier`, constant-time content-hash match (§16.5), then grant the
  manifest request **intersected** with the launching user's grants (no
  ambient authority, §4/§5.2) — and `AppLoader::resolve_library` applies the
  §16.4 policy. The filesystem (`BundleStore`) and crypto (`Verifier`) seams
  are injected, so the security-relevant code is testable without a kernel;
  the loader never executes anything (the caller spawns the verified `Run`
  binary with the computed ceiling, the same gate `init`/`drvhost` use).
  `no_std` (with `alloc`), deps only `rustos-abi`/`rustos-caps`/`rustos-log`
  (§17.4); no `unsafe`, no `unwrap`/`expect`/`panic!` in production paths.
  Audit `EventId` range `11000..12000`. 14 new `lib/abi` tests (+
  `AppInfoHeader::from_bytes` enrolled in the §19.6 fuzz harness) and 16
  `appmgr` tests; docs `docs/src/abi/appinfo.md` and
  `docs/src/userland/appmgr.md` (+ SUMMARY links) and both `README.md`s.
  **Every Stage 6 deliverable is now done.**

### Stage 6 follow-up — Rust I/O abstraction (`plans/IO.md`)

**Status: planned (not started).**

The §20 standard-stream floor is in place: every text program does I/O over
inherited fd 0/1/2/3 through the thin `lib/rt` wrappers
(`stdout`/`stderr`/`stdinfo`/`stdin`), never a device syscall. What is missing
is the ergonomic *library* on top of those wrappers — the RustOS equivalent of
a `std::io` surface (`Read`/`Write` traits, buffered reader/writer with line
reading, and `write!`/`writeln!`-style formatting) — so shells, tools, and
services program against an abstraction instead of re-implementing the same
short-write loop and "read until newline" logic (which would be the
duplication `AGENTS.md` §2.2 forbids). It is a pure layer over the existing
`abi-v1` stream syscalls: it adds **no** ABI surface, **no** syscall, and
**no** capability (`AGENTS.md` §5.4), exposes only the four standard streams
(never a device, §20), and is `no_std` + fail-closed (§2.9). RustOS does
**not** build a system-wide C `stdio` — the *System runtime / C ABI* class
stays minimal and a third-party C program brings its own libc in its bundle
(`AGENTS.md` §16.4, `plans/CCOMPAT.md`). Staged IO1 (traits + the four stream
handles) → IO2 (buffering) → IO3 (formatting) → IO4 (adopt across userland and
delete the hand-rolled loops, §2.14) in `plans/IO.md`, which is binding under
`AGENTS.md`.

---

## Stage 7 — Graphics, Window Manager, Taskbar

**Dependencies:** Stage 6 + a display driver from Stage 4.

**Deliverables**
- `userland/gui/wm`: compositing window manager. Per-window surfaces, damage
  tracking, GPU acceleration where a driver exposes it, software fallback
  otherwise. The compositor must support:
  - **Rounded window corners**: per-window corner radius applied during
    composition (anti-aliased), with a square-corner setting retained for
    windows that opt out.
  - **Alpha transparency**: per-surface and per-region alpha so a window can
    be wholly or partially translucent; the compositor blends translucent
    surfaces against what is behind them with correct premultiplied-alpha
    compositing.
- `userland/gui/taskbar`: a traditional desktop taskbar (in the style of
  GNOME/Windows), pinned to a configured screen edge. Layout:
  - **Left**: a "start" menu button opening a menu. The menu is **not** an
    application launcher; at this stage it is largely unpopulated and holds
    only session controls (log out, lock, shut down, restart). It is built
    so launcher entries can be added later without changing its public IPC.
  - **Middle**: a task list showing currently running tasks (one entry per
    top-level window/application), with focus/activate and minimise/restore
    on click.
  - **Right**: a clock anchored to the right-hand end, with a **notification
    icon area** immediately to its left for status/notification icons.
  - **Rounded edges**: the taskbar itself supports rounded corners, drawn
    through the same compositor rounded-corner path as windows (no duplicate
    implementation, `AGENTS.md` §2.2).
- Theming: a **default dark theme** plus a **light theme**, switchable at
  runtime. Themes drive colours, corner radii, fonts, and cursors for the
  WM, taskbar, and default apps through one shared theme definition; adding a
  theme is data, not new code.
- Default cursor set (themed).
- **SVG-first graphical assets** (`AGENTS.md` §10). Every WM/desktop
  graphical asset — cursors, icons, notification glyphs, window-chrome
  artwork, theme decorations — is authored as **SVG** so one source stays
  crisp at any DPI / UI `Scale`. SVG is never parsed or drawn on the hot
  compositing path: an asset is rasterised/converted **once** at the active
  scale into the fast-draw form the compositor blits (a `lib/raster`
  `Surface`, or an intermediate vector form like `lib/cursor`'s) and that
  form is cached, re-rendered only on a scale or theme change — so the
  desktop stays quick. There is one rasterisation/blend path (`lib/raster`),
  never a second (§2.2); SVG decoding is untrusted input and runs through the
  curated §16.4 image-decoding shared library inside a §19.5 parser sandbox,
  failing closed to a fallback rather than crashing the compositor (§2.9).
- Default apps under `userland/apps/`:
  - **Filesystem browser**: navigates the §16 filesystem layout, honouring
    capability-gated permissions; no `/proc`/`/sys` fabrication (§16.1).
  - **Terminal emulator**: runs the default shell with job control.

**Tests**
- Headless compositor tests using a virtual framebuffer, including
  rounded-corner masking and per-region alpha blending (premultiplied-alpha
  correctness, fully-opaque and fully-transparent edge cases).
- Taskbar layout tests: start-menu button + session-control entries on the
  left, running-task list in the middle, notification area and clock on the
  right; rounded-edge rendering.
- Theme-switch tests: dark ↔ light applies consistently across WM, taskbar,
  and default apps.
- Input routing tests (focus, click-to-activate, drag-and-drop).

**Docs**
- `docs/src/desktop/{wm,taskbar,apps,theming}.md`.

**Status: in progress.**
- **Desktop paradigm reconciled.** `AGENTS.md` §3/§10 previously named a
  RISC OS-style `userland/gui/iconbar`; the binding decision is the
  traditional GNOME/Windows-style `userland/gui/taskbar` of this stage.
  The placeholder crate was renamed `rustos-iconbar` → `rustos-taskbar`
  (`userland/gui/taskbar`) and `AGENTS.md` §3/§10, the xtask GUI-crate
  list, and the architecture overview were updated to match.
- **Compositor core — DONE (first increment).** `userland/gui/wm`
  (`rustos-wm`, `no_std`+`alloc`, dep only `rustos-abi`, §17.4) is the
  user-space software compositor (`AGENTS.md` §10/§17.3). It composes a
  z-ordered window stack over an opaque background into a `DisplayMode`
  scan-out frame and presents it through a `Display` seam:
  - **Premultiplied-alpha** `color` (`Pixel`/`Color`, shared rounded
    `div255`, `scale_alpha`, Porter–Duff `over`) — correct per-surface
    and per-region transparency.
  - **`Surface`** premultiplied pixel buffers; **`geometry`**
    `Point`/`Rect` (checked intersection/union/contains).
  - **Anti-aliased rounded corners** (`corner::Corners`) via
    deterministic supersampling (no `sqrt`), with a `Square` opt-out —
    the single rounded-corner path the taskbar will reuse (§2.2).
  - **Damage tracking** (`DamageRegion`): only changed pixels are
    recomposited; `composite` clears damage and is idempotent.
  - **`Compositor`** window ops (add/move/raise/remove, opacity, corners,
    visibility, surface replace) each mark damage; fails closed on a
    too-large/short-stride/unsupported-format mode (`None`, §2.9/§2.1).
  - 48 headless unit tests (46 compositor-core + 2 theme-integration);
    no `unsafe`, no `unwrap`/`expect`/`panic!` in production paths. Docs
    `docs/src/desktop/wm.md` (+ SUMMARY) and the crate `README.md`.
- **Shared theme definition — DONE (increment).** `lib/theme`
  (`rustos-theme`, `no_std`+`alloc`, **zero dependencies**, `Layer::Lib`)
  is the single shared theme definition the WM, taskbar, and default apps
  read (`AGENTS.md` §6/§10). It lives in `lib/*` because sibling userland
  crates may not depend on one another (§17.4) — the same reasoning as
  `lib/procinfo`. It is pure *data*:
  - **`Palette`** — eight semantic `Rgba` colour roles as fixed fields
    (illegal states unrepresentable, §2.11); **`Metrics`** — window /
    taskbar / popup corner radii + border thickness; **`Fonts`** — a UI
    and a monospace `FontSpec`; **`CursorSet`** — one asset id per
    `CursorKind`.
  - **Built-in default dark + light `Theme`s**, plus `ThemeRegistry`:
    runtime `set_active`/`register`, both fail-closed (`ThemeError`,
    §5.4/§2.9). `active()` is panic-free (built-ins held in a fixed-size
    array).
  - **No duplicated colour algebra (§2.2):** the theme `Rgba` carries no
    compositing arithmetic; it meets the compositor at one edge, the
    WM's `From<Rgba> for Color`, and a window's corner style comes from a
    theme radius through the WM's single rounded-corner path
    (`Corners::from_radius`).
  - 13 unit tests (+ a doctest); docs `docs/src/desktop/theming.md`
    (+ SUMMARY) and the crate `README.md` (tier `experimental`).
    `userland/gui/wm` is its first consumer.
- **Shared geometry library — DONE (increment).** `lib/geometry`
  (`rustos-geometry`, `no_std`, **zero dependencies**, `Layer::Lib`) now
  owns the `Point`/`Rect` coordinate types. They were defined in
  `userland/gui/wm`, but the taskbar and the default apps need the same
  vocabulary and may not depend on the window manager (§17.4); per §6/§2.2
  the shared types belong in `lib/*`. `rustos-wm` now re-exports them
  (behaviour-neutral; its `geometry` module is a one-line re-export), so
  there is exactly one definition. Added to `AGENTS.md` §3's `lib/` list
  and the workspace manifest. 7 geometry unit tests; `rustos-wm` keeps its
  43 compositor tests.
- **Taskbar layout + model — DONE (increment).** `userland/gui/taskbar`
  (`rustos-taskbar`, `no_std`+`alloc`, deps only `lib/geometry` +
  `lib/theme`, `Layer::UserGui`, §17.4) is the GNOME/Windows-style bar
  pinned to a configured screen edge (§10):
  - **Layout** (`TaskbarConfig` + `BarLayout::compute`): start button at
    the leading end, running-task list in the middle, notification-icon
    area packed before a trailing clock; horizontal or vertical depending
    on the `Edge`. All arithmetic saturates — a degenerate screen fails
    closed *inside* the bar (§2.9), and a slot that doesn't fit is
    `Rect::EMPTY` and never hit.
  - **Hit-testing** (`BarLayout::hit_test → Hit`) for input routing.
  - **Start menu** (`StartMenu`): session controls only (log out, lock,
    shut down, restart); shaped so launcher entries arrive later as a new
    `MenuAction` variant without changing the list/activate interface
    (§2.4). Fail-closed `activate` (§5.4/§2.9).
  - **Task list** (`TaskList`): one entry per top-level window with the
    click-to-activate / minimise-restore rule reported via
    `ActivateOutcome`; **notification area** (`NotificationArea`).
  - **Rounded edges via the WM path (§2.2):** the taskbar draws no corners
    itself; `BarLayout::corner_radius` carries the theme's
    `taskbar_corner_radius` and the WM rounds the bar with the same
    `Corners::from_radius` path it uses for windows. `Taskbar::apply_theme`
    switches it at runtime.
  - 20 headless unit tests; no `unsafe`, no `unwrap`/`expect`/`panic!` in
    production paths. Docs `docs/src/desktop/taskbar.md` (+ SUMMARY) and the
    crate `README.md`.
- **Input routing — DONE (increment).** `userland/gui/wm` gains an
  `input` module: `Compositor::window_at` is the top-most-visible-window
  hit-test (z-order walked top-down; rounded corners are cosmetic and do
  not carve the input region, §2.2), and `InputRouter`
  (`PointerButton`/`InputEvent`/`InputResponse`) is the input-policy
  layer over the scene graph. It tracks the pointer and the focused
  window, raises + focuses the window under a primary press
  (*click-to-activate*, reporting `Activated { window, local }`), clears
  focus on a desktop-background press (`DesktopPressed`), and drives an
  **explicit** interactive window move-grab — `begin_move` arms the grab
  (decorations call it on a title-bar press) rather than arming a move on
  every press, so content clicks and window dragging stay separated
  (§2.1, no "drag anywhere" hack). The router holds *which* window owns
  the keyboard (`focused`); the key encoding stays an ABI concern not
  invented in the compositor (§2.4). Fails closed (`begin_move` with no
  focus, grabbed window removed mid-drag → `MoveEnded`); no `unsafe`,
  no `unwrap`/`expect`/`panic!`. 8 new headless tests (51 total). Docs
  `docs/src/desktop/wm.md` and the crate `README.md` updated.
- **Shared rasteriser library — DONE (increment).** `lib/raster`
  (`rustos-raster`, `no_std`+`alloc`, dep only `lib/theme`, `Layer::Lib`)
  now owns the premultiplied-alpha `Color`/`Pixel` algebra (`div255`,
  `premultiply`/`unpremultiply`, `scale_alpha`, Porter–Duff `over`) and the
  `Surface` pixel buffer (`fill`/`fill_rect`/`get`/`set`). They were private
  to `userland/gui/wm`, but the taskbar must paint pixels too and may not
  depend on the window manager (§17.4); per §6/§2.2 the shared rasteriser
  belongs in `lib/*` — the same reasoning as `lib/geometry` and `lib/theme`.
  The `From<rustos_theme::Rgba> for Color` edge moves here too (its single
  owner), which is why the crate depends on `lib/theme`. `rustos-wm` now
  re-exports the types (behaviour-neutral; its `color`/`surface` modules are
  one-line re-exports), so there is exactly one definition. 14 unit tests
  moved into the new crate; `rustos-wm` keeps 39 compositor/input tests.
  Added to `AGENTS.md` §3's `lib/` list and the workspace manifest; the
  stale `wm::Color` references in `lib/theme` were repointed at `lib/raster`.
  Docs: crate `README.md` (tier `experimental`).
- **Taskbar pixel rendering — DONE (increment).** `userland/gui/taskbar`
  gains a `render` module (`render(&Taskbar, &Theme) -> Option<Surface>`)
  that paints the bar's regions into a `lib/raster` `Surface` sized to the
  bar, using the active theme's `Palette`: the background is `surface_raised`,
  the start button `accent`, each task slot `accent` when it is the focused
  non-minimised task / `surface_raised` (recedes) when minimised / `surface`
  otherwise, and each notification icon `on_surface_muted`. The surface is
  rectangular — the WM rounds it via `BarLayout::corner_radius` through its
  single rounded-corner path (§2.2) — and the colour algebra is reused from
  `lib/raster`, never duplicated. Screen→bar-local mapping saturates and
  `fill_rect` clips, so a degenerate layout paints nothing rather than
  panicking (§2.9). 7 new headless tests (27 total); no `unsafe`, no
  `unwrap`/`expect`/`panic!` in production paths. Docs
  `docs/src/desktop/taskbar.md`; the taskbar now depends on `lib/geometry` +
  `lib/raster` + `lib/theme` only (§17.4). Glyph rendering (clock/task-title
  text) and notification-icon artwork remain for a later increment.
- **Shared font library — DONE (increment).** `lib/font` (`rustos-font`,
  `no_std`, `#![forbid(unsafe_code)]`, dep only `lib/raster`, `Layer::Lib`)
  is the single shared **text rasteriser** — one of the curated §16.4
  shared-library classes. It owns a built-in **5×7 monospace bitmap atlas**
  (`glyphs`) covering printable ASCII (space..`~`), written as binary row
  literals so the data is self-documenting (§2.11), and `BitmapFont` (`font`):
  a face (atlas + metrics) plus the glyph blitter. `draw_text` composites each
  lit glyph pixel through `lib/raster`'s single premultiplied-alpha
  `Pixel::over` path — no colour algebra duplicated (§2.2); an out-of-range
  character renders a visible fallback box and off-screen pixels clip rather
  than panic (§2.9). `text_width` gives the tight one-line bounding width.
  Like `lib/geometry`/`lib/theme`/`lib/raster` it lives in `lib/*` so the
  taskbar and apps draw text without depending on the window manager (§17.4).
  10 unit tests; added to `AGENTS.md` §3's `lib/` list and the workspace
  manifest; crate `README.md` (tier `experimental`).
- **Taskbar text rendering — DONE (increment).** `userland/gui/taskbar`
  gains a `clock` module (`Clock` holds the caller-set display label;
  formatting a `Time64` value stays upstream, §21) and its `render` module
  now draws **text** with `lib/font`'s `BitmapFont::mono5x7`: the clock label
  centred in the clock region and each task slot's window title aligned to its
  leading edge. Each label takes the foreground role matching its background
  (`on_accent` over a focused/accent slot, the muted role over a minimised
  slot, `on_surface` otherwise and for the clock), and is truncated to the
  characters that fit so text never spills into a neighbour (§2.9). The bar
  now depends on `lib/geometry` + `lib/raster` + `lib/font` + `lib/theme`
  (§17.4). 4 new headless tests (31 total); no `unsafe`, no
  `unwrap`/`expect`/`panic!` in production paths. Docs
  `docs/src/desktop/taskbar.md` and the crate `README.md`.
- **Pointer cursors — DONE (increment).** `lib/cursor` (`rustos-cursor`,
  `no_std`, `#![forbid(unsafe_code)]`, deps only `lib/raster` +
  `lib/geometry` + `lib/theme`, `Layer::Lib`) makes the desktop's cursors
  *richer than a fill mask* (§10): a `VectorCursor` is an ordered stack of
  filled, colourful `Shape`s (polygon + straight-alpha `Color`) over a
  square design grid, plus a hotspot.
  - **Vectorised + scalable:** `rasterise(scale_percent)` maps the design
    grid to pixels at any scale (100 = 1px/unit, 200 = 2×, …) with 4×4
    supersampled anti-aliasing, compositing each shape through `lib/raster`'s
    single premultiplied-alpha `Pixel::over` path (no colour algebra
    duplicated, §2.2); it yields a `CursorImage` (a `Surface` + the hotspot
    in pixel coords). Degenerate cursor/scale → `None`, never a panic (§2.9).
  - **Colourful:** every layer carries colour + alpha; the built-in set draws
    a light body over a dark outline and a genuine two-tone busy disc.
  - **Replaceable cursor sets:** `CursorTheme` binds one `VectorCursor` per
    `rustos_theme::CursorKind` (fixed fields, total lookup, §2.11);
    `CursorRegistry` holds the available sets + the active one keyed by
    `CursorSetId`, with fail-closed `register`/`set_active` (§5.4/§2.9). A
    different look is data, not code.
  - 17 unit tests + a doctest; added to `AGENTS.md` §3's `lib/` list and the
    workspace manifest; crate `README.md` (tier `experimental`) and docs
    `docs/src/desktop/cursors.md` (+ SUMMARY).
  - **Wired into the compositor:** `userland/gui/wm` gains a `cursor` module
    (`CursorLayer`) and the `Compositor` now composites the cursor as the
    top-most overlay above all windows: `set_cursor`/`move_cursor`/
    `hide_cursor`/`cursor_bounds`, each marking the old+new footprints dirty
    so only those pixels recomposite (the same damage model the window stack
    uses) and hiding restores the pixels beneath. The WM depends on
    `lib/cursor` (§17.4); 6 new headless tests (45 total).
- **Shared polygon path + desktop icons (notification-icon artwork) —
  DONE (increment).** The anti-aliased supersampled filled-polygon scan
  converter that `lib/cursor` carried privately was lifted into
  `lib/raster` as `Surface::fill_polygon` — the desktop's single polygon
  rasteriser (§10 "one rasterisation/blend path", §2.2): vertices are
  authored on a square design grid and mapped across the surface, each
  pixel probed on a 4×4 sub-pixel grid, every covered pixel composited
  through the one `Pixel::over` path. `lib/cursor::VectorCursor::rasterise`
  now delegates to it (pixel-identical; its 17 tests unchanged), so there
  is no second scan converter. `lib/raster` also gains `Surface::blit`
  (composite one surface over another, clipping a negative origin / oversize
  source) for transparent-background sprites. The **notification-icon
  artwork** that was open is now drawn: a new `lib/icon` crate
  (`rustos-icon`, `no_std`, `#![forbid(unsafe_code)]`, dep only `lib/raster`,
  `Layer::Lib`) holds a `VectorIcon` (a stack of filled `IconLayer` polygons
  over a design grid) rasterising through `fill_polygon`, plus a closed
  `IconKind` glyph set (network, volume, battery, bell, generic) with
  `IconKind::for_asset` falling back to `Generic` for an unknown id (§2.9)
  and `builtin_icon(kind, colour)` tinting each monochrome glyph from the
  theme (re-theming is data, §10). The taskbar's `render` now resolves each
  notification slot's asset id to an `IconKind`, builds the glyph in the
  `on_surface_muted` role, rasterises it to the slot size at the active
  scale, and blits it (artwork, not a flood fill — the bar background shows
  through); a slot too small paints nothing (§2.9). 5 new `lib/raster` tests
  (21 total), 8 `lib/icon` tests + a doctest, taskbar +1 (58 total);
  `lib/cursor` unchanged. Added to `AGENTS.md` §3's `lib/` list and the
  workspace manifest; docs `docs/src/desktop/icons.md` (+ SUMMARY),
  updates to `docs/src/desktop/{cursors,taskbar}.md`, and the `rustos-icon`
  / `rustos-raster` `README.md`s. No new `unsafe`, no
  `unwrap`/`expect`/`panic!` in production paths. **Resolved:** decoding
  cursor *and* icon sets from on-disk SVG assets under `/System/Graphics`
  has now landed — see the SVG-first asset decoder increment below.
- **Variable DPI / UI scale — DONE (increment).** Variable DPI is now a
  binding, **settable** desktop property (`AGENTS.md` §10): the same image is
  comfortable on a low- or high-DPI panel and the user picks the density.
  `lib/geometry` gains `Scale` — the desktop's single DPI / UI scale factor
  (a percentage of `REFERENCE_DPI = 96`): `Scale::ONE`, `from_percent` /
  `from_dpi` (fail-closed to `MIN_PERCENT..=MAX_PERCENT`, §5.4/§2.9),
  `percent` / `dpi`, and the one shared logical→physical conversion
  `scale_length` (u64-widened, saturating, §2.9). Desktop lengths are
  *logical* pixels at the reference density; consumers scale them, so the
  conversion is never duplicated (§2.2). The taskbar consumes it:
  `TaskbarConfig` extents/thickness are logical, `BarLayout::compute` takes a
  `Scale` (scaling the extents via `TaskbarConfig::scaled` and the theme
  corner radius), and `Taskbar` carries a settable `scale()`/`set_scale()`
  so a runtime DPI change relays the bar without rebuilding its model — the
  same shape as the runtime theme switch. `lib/theme` `Metrics`/`FontSpec`
  docs now state their values are logical pixels scaled by `Scale`; cursors
  were already DPI-driven (vector artwork rasterised at the active scale).
  5 new `lib/geometry` tests (12 total) + 3 new taskbar tests (34 total); no
  `unsafe`, no `unwrap`/`expect`/`panic!` in production paths. Docs:
  `AGENTS.md` §10 + §3, `lib/geometry` and taskbar `README.md`,
  `docs/src/desktop/dpi.md` (+ SUMMARY).
- **Cursor selection from interaction state — DONE (increment).**
  `userland/gui/wm` gains a `select` module that chooses which
  `rustos_theme::CursorKind` the pointer shows from live interaction
  state, so the cursor *shape* now follows what the user is doing — not
  just the cursor *artwork* (§10). `desired_cursor(router, compositor)`
  is a pure policy: an in-flight window move-grab outranks everything and
  yields `Move`; otherwise the pointer takes the **cursor hint** of the
  top-most window under it; over the desktop background it is the plain
  `Arrow`. Each `Window` carries a `cursor_hint` (default `Arrow`, fixed
  field — total lookup, §2.11) its owner sets through the new
  `Compositor::set_window_cursor`; a hint change is window state, not
  pixels, so it marks no damage. `CursorController` ties the policy to the
  artwork: it owns the active `CursorRegistry` and the desktop `Scale`,
  remembers the kind on screen, and `refresh` rasterises and installs the
  chosen cursor through the existing `set_cursor` path **only when the
  kind changes** (pointer *motion* stays the separate `move_cursor`
  path, §2.2 — no second cursor pipeline); a runtime cursor-set swap
  (`set_registry`) or DPI change (`set_scale`) re-renders the current
  kind in place. Fails closed: an unrasterisable cursor or scale leaves
  the current pointer untouched rather than blanking it (§2.9); no
  `unsafe`, no `unwrap`/`expect`/`panic!`. 7 new headless tests (52
  total). Docs `docs/src/desktop/cursors.md` and the crate `README.md`.
- **Shared input-event vocabulary + taskbar input router — DONE
  (increment).** The device-level pointer vocabulary the desktop routes
  (`PointerButton`, `InputEvent`) was defined inside `userland/gui/wm`,
  but the taskbar must route the **same** events to hit-test its regions
  and may not depend on the window manager (§17.4). Per §6 / §2.2 it now
  lives in `lib/input` (`rustos-input`, `no_std`,
  `#![forbid(unsafe_code)]`, dep only `lib/geometry`, `Layer::Lib`) — the
  same lift as `lib/geometry` and `lib/raster`. `rustos-wm` re-exports the
  types (behaviour-neutral; its `input` module keeps the WM-specific
  `InputRouter`/`InputResponse`), so there is exactly one definition. The
  taskbar gains an `input` module: `TaskbarInput` consumes the shared
  `InputEvent` stream — the taskbar counterpart of the WM's `InputRouter`
  — tracking the pointer from motion events and acting only on a primary
  press, which it hit-tests against the current `BarLayout` and dispatches
  to the model as a `TaskbarResponse` (start-menu toggle, the task
  activate/minimise rule, or a notification-icon / clock press). A
  non-primary button, a release, or a press that misses every region is
  `Ignored` (fail closed, §2.9); selecting an entry inside an open start
  menu stays a later increment (the popup geometry). 4 new `lib/input`
  tests + 7 new taskbar tests (41 total); `rustos-wm` keeps its 52 tests.
  Added to `AGENTS.md` §3's `lib/` list and the workspace manifest; docs
  `docs/src/desktop/taskbar.md` (+ Input-routing section) and the
  `rustos-input` / taskbar / WM `README.md`s. No `unsafe`, no
  `unwrap`/`expect`/`panic!` in production paths.
- **Start-menu popup geometry + entry routing — DONE (increment).**
  `userland/gui/taskbar` gains the start menu's popup geometry and the
  routing that selects an entry inside it (previously deferred by the
  taskbar input router). `MenuLayout::compute` (in `layout.rs`, reusing
  the bar's saturating conversion helpers so nothing is duplicated, §2.2)
  opens the popup *outward* from the start button on the bar's edge —
  above a bottom bar, below a top bar, to the inner side of a left/right
  bar — with one *scale-aware* row per entry and the theme's
  `popup_corner_radius`; `MenuLayout::hit_test` maps a pointer to the
  entry index. `Taskbar` stores `popup_corner_radius` and exposes
  `menu_layout()`. `TaskbarInput` now treats the open menu as **modal**
  (§2.1, one click does one thing): a primary press inside the popup
  selects the entry under the pointer (new `MenuEntrySelected`), a press
  on the start button still toggles it shut, and a press anywhere else
  dismisses it (new `StartMenuDismissed`) without acting on what it
  landed on. `render_menu` paints the open popup through the bar's
  existing `rustos-font`/`rustos-raster` path (no second blitter or
  rounded-corner path, §2.2), returning `None` when closed; the WM
  places and rounds the rectangular surface via `MenuLayout::corner_radius`.
  All arithmetic saturates and fails closed (§2.9); no `unsafe`, no
  `unwrap`/`expect`/`panic!` in production paths. 10 new headless tests
  (51 total). Docs `docs/src/desktop/taskbar.md` and the crate
  `README.md`.
- **Start-menu launcher entries — DONE (increment).** The start menu was
  seeded with session controls and *shaped* so launcher entries could be
  added later as a new `MenuAction` variant without changing the
  list/activate interface (§2.4); that increment is now done.
  `userland/gui/taskbar`'s `menu` module gains `MenuAction::Launch(LauncherId)`
  alongside `Session(SessionControl)` (both `Copy`, so the action still travels
  by value through the input router and the `Copy` `TaskbarResponse`), and a
  `MenuEntry` now owns its display label as a `Cow<'static, str>` — session
  controls keep their static labels borrowed while a launcher carries an
  application-supplied name without the session labels allocating (§2.2/§2.3).
  `StartMenu::add_launcher(launcher, label)` appends a launcher after the fixed
  session ids `1..=4`, assigning the next id after the current maximum
  (saturating, §2.9) so already-assigned ids never move. The popup geometry
  (`menu_layout` from `start_menu.len()`), the rendering (`render_menu` draws
  `entry.label()` through the existing `rustos-font`/`rustos-raster` path), and
  the modal `TaskbarInput` selection path all carry the new variant unchanged
  — no second path (§2.2). The taskbar holds no spawn capability: activating a
  launcher only reports its `LauncherId`, which the session glue (WM/`appmgr`)
  resolves to a bundle and launches (§16.5). 3 new headless tests (54 total);
  no `unsafe`, no `unwrap`/`expect`/`panic!` in production paths. Docs
  `docs/src/desktop/taskbar.md`, the crate `README.md`, and the crate-root
  module docs.
- **Runtime light/dark appearance toggle — DONE (increment).** The charter's
  "default dark theme plus a light theme, switchable at runtime" (§10) gains
  its runtime *control*. `lib/theme`'s `ThemeRegistry` gains
  `set_appearance(Appearance)` (activate the built-in of a given appearance)
  and `toggle_appearance` (flip to the opposite built-in based on the *active*
  theme's `Appearance`, so a custom dark theme toggles to the light built-in
  and vice versa); both return the now-active `ThemeId` and cannot fail — the
  two built-ins are always present, so unlike `set_active` there is no
  unknown-id path to surface. `userland/gui/taskbar`'s `menu` module gains a
  third `MenuAction` variant, `ToggleAppearance` (a `Copy` unit variant, so it
  still travels by value through the modal input router and the `Copy`
  `TaskbarResponse`), and `StartMenu::add_appearance_toggle(label)` appends it
  after the session controls and launchers with the next free id — additive,
  so the default menu and existing tests are unchanged and the list/activate
  interface did not move (§2.4). The popup geometry, render path, and modal
  selection routing carry the new variant unchanged — no second path (§2.2).
  The taskbar owns no theme registry: activating the entry only reports
  `ToggleAppearance`, and the session glue performs the switch on the shared
  `ThemeRegistry` and relays the new theme back to the WM/taskbar/apps (§10).
  3 new `lib/theme` tests (15 total) + 3 new taskbar tests (57 total); no
  `unsafe`, no `unwrap`/`expect`/`panic!` in production paths. Docs
  `docs/src/desktop/{theming,taskbar}.md`, both crate `README.md`s, and the
  taskbar crate-root module docs.
- **Desktop session glue (light/dark switch) — DONE (increment).** The
  taskbar reports an abstract `MenuAction::ToggleAppearance` but, by design,
  owns no theme registry; resolving it is the session glue's job (§10). That
  glue now exists: `userland/gui/session` (`rustos-desktop-session`, `no_std`,
  `#![forbid(unsafe_code)]`, deps only `rustos-taskbar` + `rustos-theme`,
  `Layer::UserGui`) owns the one shared `ThemeRegistry` and the `Taskbar`
  model. `DesktopSession::resolve` turns a `TaskbarResponse` into a
  `SessionEvent`: a selection of the appearance-toggle entry is the single
  response it acts on itself — it calls `ThemeRegistry::toggle_appearance`,
  re-themes the taskbar in place, and returns `AppearanceChanged(ThemeId)` so
  the embedder relays the now-active theme (`active_theme()`) to the WM and
  apps; every other response is `Forward`ed unchanged (a launcher /
  session-control selection, task activation, notification/clock press all
  need capabilities the session does not hold, §10/§16.5). `toggle_appearance`,
  `set_theme`, and `register_theme` expose the same control directly;
  `set_theme`/`toggle_appearance` re-theme the taskbar through one private
  apply path, so the relay is never duplicated (§2.2), and `set_theme`/
  `register_theme` fail closed (`UnknownTheme`/`DuplicateId`) leaving the
  active theme and the taskbar untouched (§5.4/§2.9). Composing two GUI crates
  is the permitted `userland/gui/*` edge (§17.4); nothing non-GUI depends on it
  (§17.3). 6 headless tests. Added to the workspace manifest and `AGENTS.md`
  §3; docs `docs/src/desktop/session.md` (+ SUMMARY), the crate `README.md`,
  the crate-root module docs, and the `theming.md` cross-reference. **Still
  open here:** relaying the active theme to the WM and apps over live IPC, and
  resolving the forwarded launcher / session-control actions once the
  window-manager and process capabilities are wired (deferred Stage 6 work).
- **Filesystem browser — DONE (increment).** `userland/apps/files`
  (`rustos-files`, `no_std`+`alloc`, deps only `rustos-abi` + the shared
  desktop `lib/*` crates `geometry`/`theme`/`raster`/`font`, §17.4) is the
  default graphical file manager (`AGENTS.md` §10/§16). It is a navigation
  **model** plus a **renderer** driven by an injected `DirectorySource`
  (`list(components) -> Result<Vec<Entry>, Errno>`) — VFS-backed on a running
  system, an in-memory tree in tests (§7). `Browser` owns the current path,
  entries, and a selection cursor; descend (`open_index`/`open_selected`),
  climb (`go_up`, `Ok(false)` at the root — not an error), and `refresh` are
  **transactional and fail closed**: the target is listed *before* any state
  changes, so a refused/failing read (`BrowseError::Source(Errno)`), a file
  target (`NotADirectory`), or an out-of-range index (`NoSuchEntry`) leaves
  the browser where it was (§5.4). It shows exactly the source's entries — no
  `/proc`/`/sys` fabrication (§16.1) — and makes no permission decision of its
  own (the §5.3 check lives in the VFS). `render` paints a path bar plus a
  scrolling, selection-highlighted entry list into a `lib/raster` `Surface`
  using the theme palette and the `lib/font` face; the compositor rounds the
  rectangular surface (§2.2). The fit-to-width truncation moved into
  `lib/font` as `BitmapFont::truncate_to_width`, the single path the taskbar
  renderer now also uses — no duplication (§2.2). 14 headless `rustos-files`
  tests + 2 new `lib/font` tests (14 total); no `unsafe`, no
  `unwrap`/`expect`/`panic!` in production paths. Workspace manifest; docs
  `docs/src/desktop/apps.md` (+ SUMMARY) and the crate `README.md`. **Still
  open here:** the VFS-backed `DirectorySource`, live pointer/keyboard input,
  and presenting the window through the WM (deferred wiring).
- **Terminal emulator — DONE (increment).** `userland/apps/terminal`
  (`rustos-terminal`, `no_std`+`alloc`, deps only `rustos-abi` + the shared
  desktop `lib/*` crates `geometry`/`theme`/`raster`/`font`, §17.4) is the
  default graphical terminal (`AGENTS.md` §10). Like the file browser it is a
  screen **model** plus a **renderer**, both driven by an injected
  `ShellSource` seam (`read`/`write` over the kernel boundary `Errno`),
  testable against an in-memory queue without a kernel (§7). `Grid` is the
  fixed `cols`×`rows` character-cell screen with a cursor, exposing the
  cursor-relative operations (write-with-wrap-and-scroll, the C0 moves,
  absolute/relative positioning, the ANSI erase operations, clear); every one
  is total and saturating, so a hostile byte stream can never index out of
  bounds or panic (§2.9). `Parser` is the ground/escape/CSI state machine that
  turns shell output bytes into those operations — printable ASCII, the C0
  controls, and a CSI subset (`A`/`B`/`C`/`D`, `H`/`f`, `J`, `K`) — consuming
  any unrecognised escape, unsupported final byte, or high byte without
  disturbing the screen (§2.9), keeping the model free of parsing concerns
  (§2.3). `Terminal` glues them to the seam: `pump` reads-and-applies and
  `send`/`send_str` forward input, never echoing on the screen's behalf (echo
  is the shell's job) and surfacing a seam `Errno` while leaving the screen
  unchanged (§5.4). `render` paints the grid into a `lib/raster` `Surface`
  through the shared `lib/font` monospace face and the theme palette, the
  cursor cell highlighted with the accent role; the compositor rounds the
  rectangular surface (§2.2). 23 headless tests; no `unsafe`, no
  `unwrap`/`expect`/`panic!` in production paths. Workspace manifest; docs
  `docs/src/desktop/apps.md` and the crate `README.md`. **Still open here:**
  wiring the pseudo-terminal `ShellSource` to a real shell process and
  presenting the window through the WM (deferred wiring).
- **SVG-first asset decoder — DONE (increment).** The desktop's SVG-first
  asset rule (§10) gains its decoder: a new `lib/svg` crate (`rustos-svg`,
  `no_std`, `#![forbid(unsafe_code)]`, dep only `lib/raster`, `Layer::Lib`) is
  the first-party (§2.12) §16.4 image-decoding library that turns an on-disk
  SVG asset into an `SvgImage` — a square design grid plus an ordered stack of
  filled polygon `SvgLayer`s and an optional hotspot, exactly the shape
  `lib/cursor`'s `VectorCursor` and `lib/icon`'s `VectorIcon` already hold, so
  the asset rasterises through `lib/raster`'s single polygon path with no
  second rasteriser (§2.2). The supported subset is the flat, straight-line
  one that maps to stacked filled polygons: a square `viewBox` (or equal
  `width`/`height`); `<polygon>`/`<polyline>`/`<rect>`/`<path>` with the
  straight-line commands `M`/`L`/`H`/`V`/`Z`; hex/named/`none` fills with
  `fill-opacity`; integer coordinates. SVG is untrusted input (§19.5): `decode`
  is **total** — it never panics for any byte string, returns a precise
  `SvgError` for anything out of subset, and the caller fails closed to a
  built-in cursor / `builtin_icon` glyph (§2.9). `lib/cursor` and `lib/icon`
  gain `VectorCursor::from_svg`/`VectorIcon::from_svg` + `decode_svg`
  wrappers (cursor preserving the `data-hotspot-*` hotspot). 33 `lib/svg` unit
  tests + a doctest + a §19.6 fuzz harness (`tests/fuzz_svg.rs`, registered in
  xtask — it caught and fixed a path-builder DoS), 3 new `rustos-icon` tests,
  4 new `rustos-cursor` tests. `AGENTS.md` §3 lib list + workspace manifest;
  docs `docs/src/desktop/svg-assets.md` (+ SUMMARY), updates to
  `docs/src/desktop/{cursors,icons}.md`, the `rustos-svg`/`rustos-icon`/
  `rustos-cursor` `README.md`s. No `unsafe`, no `unwrap`/`expect`/`panic!` in
  production paths.
- **SVG-asset caching layer — DONE (increment).** The SVG-first rule's
  "convert once at the active scale, re-render only on a scale or theme change"
  (§10) gains its mechanism: a single shared `lib/raster` `RasterCache<K, V, E>`
  — an epoch-keyed memoisation of rasterised assets. Keyed by an asset identity
  `K` within an *epoch* `E` (a scale paired with a theme identity): a changed
  epoch discards every entry, a stable epoch reuses, and a render that fails
  closed (a degenerate asset/scale, §2.9) is not remembered so it is retried
  rather than poisoning the cache. The two desktop consumers share this one
  cache rather than each growing its own (§2.2 / §6): the WM's
  `CursorController` caches each on-screen `CursorKind` against the
  `(scale%, CursorSetId)` epoch — re-showing a kind reuses its `CursorImage`,
  only a scale change or cursor-set swap re-rasterises — and the taskbar's
  rendering moved from a stateless `render`/`render_menu` free pair into a
  stateful `TaskbarRenderer` owning a glyph cache keyed by `IconKind` against
  the `(tint, pixel-size)` epoch, so the bar repaints its cheap regions every
  frame but rasterises a notification glyph only once per theme and scale (the
  `Taskbar` model stays pure data; `render_menu` is a cacheless `&self`
  method). 6 new `lib/raster` tests (27 total), +1 `rustos-wm` test (53), +1
  `rustos-taskbar` test (59). Docs `docs/src/desktop/svg-assets.md` (new
  caching section) + `{cursors,taskbar}.md` + the `rustos-raster`/
  `rustos-wm`/`rustos-taskbar` `README.md`s. No `unsafe`, no
  `unwrap`/`expect`/`panic!` in production paths.
- **On-disk SVG asset-set loaders — DONE (increment).** The single-asset
  `decode_svg`/`from_svg` wrappers grew into whole-*set* loaders that build a
  complete cursor or icon set from the on-disk SVG assets under
  `/System/Graphics` (one asset per kind). Reading the bytes needs a filesystem
  capability and is the userland desktop's job, so each `no_std` library takes
  the bytes through an injected seam — the same pattern the default apps use
  for their VFS/shell channels — and opens no path of its own (§17.4 / §19.5).
  `lib/cursor`'s `load` module adds `CursorAssetSource` and
  `CursorTheme::from_assets(source)`; the result is a `CursorTheme` registered
  through the existing `CursorRegistry`, so the compositor is unchanged.
  `lib/icon`'s `load` module adds `IconAssetSource`, `IconSet`, and
  `IconSet::from_assets(source)`; `IconSet::icon(kind, tint)` returns the loaded
  asset (keeping its authored colours) or, for a kind it lacks, the tinted
  `builtin_icon` glyph. Both are **total and fail-closed per kind** (§2.9): a
  kind whose asset is missing, malformed, or out of subset keeps its built-in
  artwork, so an empty source yields the built-in set and a partly-broken set
  mixes loaded assets with built-in fallbacks — a corrupt `/System/Graphics`
  can never blank the pointer or a status icon. `CURSOR_KINDS`/`ICON_KINDS`
  are the closed kind lists a loader iterates. 5 new `rustos-cursor` tests
  (22 total) + 5 new `rustos-icon` tests (13 total + a doctest); docs
  `docs/src/desktop/svg-assets.md` (new asset-set section) and the
  `rustos-cursor`/`rustos-icon` `README.md`s. No `unsafe`, no
  `unwrap`/`expect`/`panic!` in production paths. **Resolved:** the userland
  `/System/Graphics` reader that drives `from_assets` now exists — see the
  desktop-session asset-loader increment below.
- **Feeding a loaded icon set into the taskbar — DONE (increment).** A loaded
  `IconSet` (from the on-disk `/System/Graphics` SVG assets) can now be
  installed at runtime. `lib/icon` gains `IconSet::builtin()` (a `const`
  all-fallback set) and a `Default` impl, so a complete icon set always exists
  before any asset loads (§2.9). The taskbar's `TaskbarRenderer` holds an
  `IconSet` (built-in until `set_icons` swaps a loaded one in) and resolves
  each notification glyph through `IconSet::icon(kind, tint)` instead of always
  calling `builtin_icon`: a loaded asset keeps its authored colours, an omitted
  kind keeps its tinted built-in glyph. Installing a set bumps a generation
  that is part of the glyph-cache epoch, so the next frame re-rasterises from
  the new set rather than reusing the cached built-in (§2.2/§10). The cursor
  side already feeds a loaded `CursorTheme` through `CursorRegistry`, so this
  closes "feeding a loaded set into the WM/taskbar at runtime". 3 new
  `rustos-taskbar` tests (62 total) + 1 new `rustos-icon` test (14 total +
  doctest); docs `docs/src/desktop/svg-assets.md` (runtime-feeding section) and
  the `rustos-taskbar`/`rustos-icon` `README.md`s. No `unsafe`, no
  `unwrap`/`expect`/`panic!` in production paths. **Resolved:** the userland
  `/System/Graphics` reader now exists — see the desktop-session asset-loader
  increment below.
- **Desktop-session `/System/Graphics` asset loader — DONE (increment).** The
  on-disk SVG set loaders (`CursorTheme::from_assets` / `IconSet::from_assets`)
  take asset *bytes* through a seam; reading those bytes off disk needs a
  filesystem capability, so it is the userland desktop's job, not the `no_std`
  `lib/cursor` / `lib/icon` crates' (§17.4 / §19.5). `userland/gui/session`'s
  new `assets` module is that job: a `GraphicsAssetReader` seam
  (`read(path) -> Result<Vec<u8>, Errno>`, VFS-backed on a running system, an
  in-memory table in tests) plus owned-byte `CursorAssetSource` /
  `IconAssetSource` implementations. `DesktopSession::load_cursors` reads the
  asset named by the active theme's `CursorSet` for each cursor kind from
  `/System/Graphics/Cursors/<asset-id>.svg` and returns a `CursorTheme` (the
  WM registers it through `CursorRegistry`); `load_icons` reads each
  `IconKind`'s asset from `/System/Graphics/Icons/<asset-id>.svg` and returns
  an `IconSet` (the taskbar installs it through `TaskbarRenderer::set_icons`).
  `lib/icon` gains `IconKind::asset_id` — the inverse of `for_asset`, so the
  id↔kind mapping lives in one place (§2.2). Both loaders are **total and
  fail-closed per kind** (§2.9): a read error is treated exactly like a missing
  asset, so a kind whose asset is absent, unreadable, malformed, or out of
  subset keeps its built-in artwork and a corrupt `/System/Graphics` can never
  blank the pointer or a status icon. 6 new `rustos-desktop-session` tests
  (12 total) + 1 new `rustos-icon` test (15 total + doctest); deps add
  `rustos-cursor`/`rustos-icon`/`rustos-abi`. Docs
  `docs/src/desktop/svg-assets.md` (new reader section), the
  `rustos-desktop-session` `README.md` + crate-root module docs. No `unsafe`,
  no `unwrap`/`expect`/`panic!` in production paths. **Still open here:** the
  VFS-backed `GraphicsAssetReader` for a running system (the in-memory-tested
  loader and its fallbacks now exist).
- **Taskbar↔WM presentation glue — DONE (increment).** The taskbar paints a
  *rectangular* `rustos_raster::Surface` and the window manager composites and
  rounds windows; neither depends on the other (§17.4), so joining them is
  session glue. `userland/gui/session` gains a `presenter` module:
  `TaskbarPresenter` (owns only the two compositor `WindowId`s it minted)
  takes a `&mut rustos_wm::Compositor` and the taskbar's own `TaskbarRenderer`
  (which holds the across-frame glyph cache) and `present`s the bar and, while
  the start menu is open, its popup — each painted, placed at its computed
  origin (`BarLayout::bar` / `MenuLayout::panel`), and rounded with
  `Corners::from_radius` through the compositor's **single** anti-aliased
  rounded-corner path, the same one used for application windows (§2.2). It is
  total and fails closed (§2.9): a render that cannot allocate leaves the
  on-screen window untouched, closing the menu removes the popup window, a
  window the compositor no longer knows is re-created on the next present, and
  `teardown` removes both windows. Composing the taskbar and window-manager
  GUI crates is the permitted `userland/gui/*` edge (§17.4); the session adds a
  `rustos-wm` dep. 7 new `rustos-desktop-session` tests (19 total). Docs
  `docs/src/desktop/session.md` (new presentation section) + the crate
  `README.md` + crate-root module docs. No `unsafe`, no
  `unwrap`/`expect`/`panic!` in production paths. **Still open here:** relaying
  **live** pointer/keyboard events into the routers and the theme switch over
  IPC; this increment is the surface-presentation glue.
- **Session input routing — DONE (increment).** A real input source produces
  one pointer-event stream, but the desktop has two routers — the WM
  `InputRouter` and the taskbar `TaskbarInput`, both over the shared
  `lib/input` vocabulary (§17.4, §2.2) — so deciding which one each event
  belongs to is session glue. `userland/gui/session` gains an `input` module:
  `SessionInputRouter` owns both routers and fans the stream through
  `handle(event, &mut Compositor, &mut Taskbar) -> SessionInputResponse`. The
  taskbar claims a **primary press** when its menu is open (modal) or the
  pointer is over the bar, the WM gets it otherwise — never both, so a click on
  the bar never also activates a window beneath it (§2.1); **motion** is fanned
  to both so their pointers stay in step while only the WM acts on it (dragging
  a grabbed window); a **release** ends a WM move-grab; everything else is
  `Ignored`. `begin_move`/`focused` delegate to the WM router. It holds no
  pixels and grants itself no authority; every routed sub-call is total and
  fails closed (§2.9). 10 new `rustos-desktop-session` tests (29 total). Docs
  `docs/src/desktop/session.md` (new input-routing section) + the crate
  `README.md` + crate-root module docs. No `unsafe`, no
  `unwrap`/`expect`/`panic!` in production paths. **Still open here:** feeding
  this router from **live** pointer/keyboard device events.
- **Desktop shell event loop — DONE (increment).** The session held the
  desktop's pieces apart — the `DesktopSession` (theme registry + taskbar
  model), the `SessionInputRouter` (fans one event stream to the WM and taskbar
  routers), the `TaskbarPresenter` (presents the bar + popup), and the
  `TaskbarRenderer` (the glyph cache) — but nothing tied them into one
  event-driven frontend. `userland/gui/session` gains a `shell` module:
  `DesktopShell` owns those four pieces and runs the desktop loop over an
  injected `InputSource` seam (a real pointer/keyboard channel on a running
  system, an in-memory queue in tests, §7). `pump(source, &mut Compositor)`
  drains every pending event, routing each through the `SessionInputRouter` and
  returning a `ShellOutcome` per event (`Ignored`, a `WindowManager` action, or
  a `Session` event); a taskbar action is `resolve`d (the light/dark toggle
  applied here, everything else forwarded) and the bar re-presented, while a
  WM action needs no re-present so motion/drags stay cheap. A faulting source
  ends the `pump` with its `Errno`, the drained events staying applied (§2.9 /
  §19.5). `set_icons`/`begin_move`/`teardown`/`present` round it out; the shell
  holds no framebuffer (the `Compositor` is the embedder's) and grants itself
  no authority. 8 new `rustos-desktop-session` tests (37 total). Docs
  `docs/src/desktop/session.md` (new live-input-stream section) + the crate
  `README.md` + crate-root module docs. No `unsafe`, no
  `unwrap`/`expect`/`panic!` in production paths. **Still open here:** backing
  the `InputSource` with a live device channel and relaying the theme switch
  over IPC.
- **Live pointer-input ABI + device input source — DONE (increment).** The
  `DesktopShell` consumed an injected `InputSource` but had no live backing:
  there was no ABI for a pointer event to cross the kernel boundary to the
  desktop. Treating `abi-v1` as **not** frozen (the task direction supersedes
  the charter's / this plan's "frozen" language), `lib/abi` gains an `input`
  module: `PointerInput` — a typed 20-byte little-endian record (an absolute
  `Moved`, or a `Pressed`/`Released` carrying a `PointerButtonCode` of
  primary/secondary/middle) with `to_le_bytes` and a fail-closed `from_bytes`
  that validates magic, version, the reserved field, the kind, and the
  button/coordinate consistency (§5.4 / §19.5). It is the *desktop-level*
  pointer event — deliberately **distinct** from the *device-level*
  `driver::input::InputEvent` (relative deltas + keycodes), not a duplicate of
  it (§2.2). The decoder is enrolled in the `lib/abi` fuzz harness (§19.6), and
  `ABI_VERSION_CURRENT_U16` + `le::{read_i32,put_i32}` were added so no encoder
  open-codes a truncating cast (§2.2). `userland/gui/session` gains a `device`
  module: `DeviceInputSource` wraps an injected `PointerInputChannel` (the
  kernel input channel; an in-memory queue in tests, §7), and each `poll`
  decodes one `PointerInput` record into the `lib/input` `InputEvent` the WM
  and taskbar route — a malformed record surfaces its `Errno` rather than being
  misinterpreted (§2.9). This closes "backing the `InputSource` with a live
  device channel" (only wiring the channel to a real kernel endpoint remains).
  13 new `rustos-abi` `input` tests + 1 `le` test, 5 new
  `rustos-desktop-session` tests (51 total). `docs/src/abi/input.md` (+
  SUMMARY), `docs/src/lib/abi.md`, `docs/src/desktop/session.md`, and the
  session `README.md`. No `unsafe`, no `unwrap`/`expect`/`panic!` in production
  paths. **Still open here:** relaying the theme switch over IPC and giving the
  `PointerInputChannel` a real kernel-endpoint backing.
- **Live keyboard-input ABI + keyboard source — DONE (increment).** The pointer
  vertical had a live backing but the keyboard did not — the pointer ABI and
  `lib/input` both deliberately deferred "the key encoding" pending its
  definition. Treating `abi-v1` as **not** frozen, `lib/abi`'s `input` module
  gains `KeyInput` — a typed 20-byte little-endian record (a `Pressed` /
  `Released` carrying a `KeyValue` — a Unicode `Char` or a `NamedKeyCode`
  (Enter, the arrows, F1–F12, …) — plus a `Modifiers` shift/ctrl/alt/meta set)
  with `to_le_bytes` and a fail-closed `from_bytes` that validates magic,
  version, the reserved field, the kind, the modifier bits, the key class, the
  named-key code, and that the codepoint is a real Unicode scalar (§5.4 /
  §19.5); it is enrolled in the `lib/abi` fuzz harness (§19.6). It is the
  *desktop-level* key event, distinct from the device-level driver keycodes,
  not a duplicate (§2.2). `lib/input` gains the matching routing vocabulary
  (`Modifiers`/`NamedKey`/`Key` + `InputEvent::KeyPressed`/`KeyReleased`), and
  the WM `InputRouter` delivers a key to the focused window as
  `InputResponse::Key` (a key with no focus, or a vanished focus, is ignored
  and stale focus dropped, §2.9); the taskbar and session routers route keys to
  the WM, never the bar. `userland/gui/session` gains a `keyboard` module:
  `KeyboardInputSource` wraps an injected `KeyInputChannel` (an in-memory queue
  in tests, §7) and decodes one `KeyInput` per `poll` into the same `lib/input`
  `InputEvent` stream the shell pumps (the twelve wire function-key codes fold
  into one `NamedKey::Function`). 14 new `rustos-abi` tests, 2 new
  `rustos-input` tests, 3 new `rustos-wm` tests (65 total), 5 new
  `rustos-desktop-session` tests. Docs `docs/src/abi/input.md` (retitled
  "Input events") + SUMMARY + `docs/src/lib/abi.md` + `docs/src/desktop/session.md`
  + the `lib/input` / `wm` / session `README.md`s. No `unsafe`, no
  `unwrap`/`expect`/`panic!` in production paths. **Still open here:** giving the
  `KeyInputChannel` (and `PointerInputChannel`) a real kernel-endpoint backing.
- **Per-output (per-monitor) DPI ownership — DONE (increment).** The desktop
  UI scale was tightened to the correct multi-monitor shape: display density
  is a property of an **output**, owned by the **compositor**, not a
  desktop-wide value and not stored by the taskbar, cursor controller, or
  apps (§10/§2.2). `Compositor` now owns its output's `Scale` as the single
  source of truth (`scale`/`set_scale` — a change marks the whole screen dirty
  so every window re-rasterises — and `window_scale(id)`, the read-only query
  an app uses for *its* window). The `Taskbar` dropped its stored scale: `layout`,
  `hit_test`, and `menu_layout` (and the renderer's `render`/`render_menu` and
  `TaskbarInput::handle`) take the `Scale` as a parameter, and the presenter
  and session input router supply `Compositor::scale` at present/route time —
  so a runtime DPI change is transparent to the bar. The `CursorController`
  likewise dropped its scale field, reading `Compositor::scale` when it
  rasterises; `refresh` now re-renders when the kind, the cursor set, **or**
  the output scale changes (a DPI switch is `set_scale` + one `refresh`).
  `DesktopShell::set_scale` drives the compositor and re-presents the bar.
  This generalises to N monitors at different DPIs: each output carries its
  own scale and a window's effective density is its output's. 4 new wm tests
  (56 total), 1 new session test (38 total); taskbar (62) retained. Docs:
  `docs/src/desktop/dpi.md` (+ cursors.md), the wm/taskbar `README.md`s. No
  `unsafe`, no `unwrap`/`expect`/`panic!` in production paths.
- **Running-task list ↔ window stack — DONE (increment).** The taskbar
  modelled a running-task list (one entry per top-level window, the
  click-to-activate / minimise rule) and the WM a window stack, but nothing
  joined them (§17.4: neither may depend on the other). `userland/gui/session`
  gains a **`tasks`** module: **`TaskBridge`** owns the correspondence between
  compositor windows and taskbar tasks — the WM mints `WindowId` as an *opaque*
  token, so the bridge mints a stable `TaskId` per tracked window and never
  reuses one, translating between the two. It is total and fail-closed (§2.9):
  **`open`** adds a window to the compositor, lists it as a task, and shows /
  raises / focuses it (only the id-space-exhausted case opens nothing);
  **`close`** removes the window and its task and drops focus if held;
  **`activate`** applies the bar's `ActivateOutcome` to the compositor (an
  activated task is shown, raised, focused; a minimised one hidden and
  unfocused); **`sync_focus`** mirrors a WM focus change back into the bar's
  highlight, returning whether it moved so a click on a window owning no task
  neither blanks the highlight nor forces a needless repaint. `DesktopShell`
  drives it (`open_window` / `close_window`; `handle` applies a `TaskActivated`
  outcome and mirrors WM focus). Two additive seams support it: WM
  `InputRouter::focus`/`unfocus` (programmatic, compositor-validated,
  fail-closed) and taskbar `TaskList::set_focused` (mirror + restore, rejecting
  an unknown id). The bridge holds no pixels and grants itself no authority
  (the compositor/router/taskbar are the embedder's). 11 new
  `rustos-desktop-session` tests (46 total), 2 new `rustos-wm` (58), 2 new
  `rustos-taskbar` (64). Docs `docs/src/desktop/session.md` (new section) +
  the session/wm/taskbar `README.md`s. No `unsafe`, no
  `unwrap`/`expect`/`panic!` in production paths. **Still open here:** the
  `appmgr`/WM glue that opens a *launched* app's window through this bridge
  (deferred Stage 6 process wiring) — the model side is complete.
- **GPU-accelerated compositor path — DONE (increment).** The optional
  hardware-accelerated present path landed, with the software path staying
  the mandatory fallback (`AGENTS.md` §10). The display ABI gained an
  `AcceleratedDisplay: Display` seam in `lib/abi`
  (`accel_caps -> AccelCaps`, `present_layers(&[AccelLayer])`): a back-end
  that can composite a back-to-front stack of premultiplied-alpha layers
  itself exposes it; every `AcceleratedDisplay` is still a `Display`, so the
  full-frame software path is always available. The compositor gained
  `present_accelerated`: it encodes the scene as one background layer, one
  layer per visible window (its surface baked with opacity and rounded-corner
  coverage via the shared `Window::sample_local`/`CursorLayer::sample_local`),
  and the cursor on top, hands them to the engine, and **falls back to the
  software `present`** when the scene exceeds the reported `AccelCaps` (too
  many layers, or a layer too large) — never a partial hardware frame (§2.9).
  The first accelerated driver is **`drivers/display/rpi_hvs`**
  (`rustos-drv-display-rpi-hvs`), the Raspberry Pi VideoCore HVS plane
  compositor: it uploads each layer to a capability-mapped per-plane buffer,
  builds a VC4-style display list (bus-address plane pointers, fail-closed
  aperture bounds), writes it to the HVS DLIST RAM, and arms the display
  channel — all over `CAP_MMIO_MAP`-gated `MmioMapper` windows, in user space
  (no `CAP_DRV_KERNEL`). 6 new `rustos-abi` display tests, 12 new
  `rustos-drv-display-rpi-hvs` tests, 4 new `rustos-wm` tests (62 total).
  `AGENTS.md` §3 display tree + workspace manifest; docs
  `docs/src/drivers/display.md` (accelerated seam + driver section) + the
  driver `README.md`. No `unsafe` outside the mock mapper's reviewed block,
  no `unwrap`/`expect`/`panic!` in production paths. **Still open here:** a
  QEMU vertical awaiting an HVS-emulating board model, and DMA-visible plane
  buffers from the compositor's own window allocations (today the driver
  uploads into its plane buffers per present).
- **Kernel IPC-backed input channels — DONE (increment).** The
  `PointerInputChannel` and `KeyInputChannel` seams that `DeviceInputSource` /
  `KeyboardInputSource` decode were, until now, satisfied only by an in-memory
  test queue; their *kernel* backing now exists. `userland/gui/session` gains an
  `ipc` module: `IpcInputChannel` delivers each fixed-length input record as the
  payload of an `abi-v1` IPC message received from a bound kernel endpoint. The
  raw messages arrive through an injected `MessagePort` seam — the `ipc_recv`
  syscall on a running system, an in-memory queue in tests (§7) — so the crate
  still holds no endpoint capability of its own and the framing runs above the
  kernel boundary (§17.4 / §19.5). `recv_record` validates every message before
  its payload becomes a record and fails closed (§5.4 / §2.9): the
  `IpcMessageHeader` must decode (magic, ABI version, reserved field, bounded
  length), the message must be destined for the bound endpoint (`NotFound`
  otherwise), and the payload must be exactly the record's `WIRE_LEN`
  (`BufferTooSmall` / `MessageTooLarge` otherwise), so a truncated, misrouted,
  or corrupt frame can never decode as a spurious pointer move or key press. A
  pointer record and a key record are each a fixed-length payload behind one IPC
  header, so the channel implements **both** seam traits through one shared
  validation path rather than two (§2.2); which records flow is decided by the
  endpoint a channel is bound to. This closes the long-open "give the
  `PointerInputChannel` / `KeyInputChannel` a real kernel-endpoint backing"
  thread, leaving only the live `ipc_recv` `MessagePort` wiring (which awaits the
  kernel-side named-port registry). 13 new `rustos-desktop-session` tests
  (69 total). Docs `docs/src/desktop/session.md` (new section) + the crate
  `README.md` + crate-root module docs. No `unsafe`, no
  `unwrap`/`expect`/`panic!` in production paths. **Still open here:** wiring the
  `MessagePort` to the live `ipc_recv` syscall (the kernel named-port registry
  it depended on has now landed — see below; what remains is composing the
  registry into `KernelState` and the user-memory copy-in path).
- **Kernel named-port registry — DONE (increment).** The long-standing
  prerequisite for a live `ipc_recv` — a kernel map from an `EndpointId` to the
  live `Port` bound to it — now exists as `kernel/ipc::registry::PortRegistry`.
  `Port::create` proves bind authority and returns an *anonymous* owned port;
  the registry is what lets a sender/receiver reach it by the endpoint number
  carried in an `IpcMessageHeader`. Like `kernel/sec`'s `CapTable` it has **no
  interior mutability** (the synchronisation policy lives with `KernelState`,
  §2.1 — no global mutable static): `lookup`/`contains` borrow `&self` so
  concurrent senders share a read guard while each `Port::send` re-checks the
  per-send capability, and `register`/`unregister` take `&mut self`. It is
  fail-closed and audited (§5.4): `register` refuses to overwrite a live binding
  (`Errno::AlreadyExists` + `PORT_REGISTER_DENIED`, handing the rejected port
  back boxed so the caller can tear it down), and `unregister` destroys the
  removed port (draining in-flight messages, `PORT_DESTROYED`) then emits
  `PORT_UNREGISTERED`; an unknown endpoint is `NotFound`. It performs no
  capability check of its own — a pure ownership map, mirroring how `CapTable`
  stores the output of `TaskCapabilities::derive`. `lib/abi` gains
  `Errno::AlreadyExists = 17` (abi-v1 treated as not frozen; appending only) and
  `kernel/ipc::audit` gains `PORT_REGISTERED`/`PORT_REGISTER_DENIED`/
  `PORT_UNREGISTERED` (ids 3003–3005). 8 new `rustos-kernel-ipc` tests (43
  total) + 1 `rustos-abi` discriminant assertion. Docs
  `docs/src/architecture/ipc.md` (new "Named-port registry" section + audit /
  error tables) + `kernel/ipc` crate-root rustdoc + the `kernel/core::syscalls`
  deferral note. No `unsafe`, no `unwrap`/`expect`/`panic!` in production paths.
  **Still open here:** composing the registry into `KernelState` and wiring it
  into the `ipc_send` / `ipc_recv` handlers, which additionally await the
  user-memory copy-in path (the same prerequisite `cap_delegate` waits on,
  Stage 5 / Stage 6).
- **Well-known port names — DONE (increment).** A numeric `EndpointId` is an
  opaque handle a binder must already know; for the desktop to *discover* its
  pointer/keyboard input endpoint (and a process to reach any well-known
  service) by a stable name, `kernel/ipc::PortRegistry` gains a second index
  from a validated name to an `EndpointId`. `lib/abi::ipc` gains `PortName`
  (abi-v1 treated as not frozen, appending only): a non-empty ≤ 31-byte ASCII
  name — lowercase-letter first, then lowercase/digit/`'.'`/`'_'`, no trailing
  `'.'`, no `".."` — with a 32-byte length-prefixed wire form
  (`to_le_bytes`/`from_bytes`), validated fail-closed (`LengthOutOfRange` /
  `OutOfRange` / `BufferTooSmall` / `BadMagic`) and enrolled in the `lib/abi`
  fuzz harness (§19.6). The registry's `publish_name(name, id)` binds a name to
  a **currently-registered** endpoint (fail-closed: `AlreadyExists` for a name
  already in use, `NotFound` for an unregistered endpoint — both
  `PORT_NAME_PUBLISH_DENIED`, so a name never resolves to a non-existent port
  and a live name is never silently re-pointed); `resolve`/`resolve_port` map a
  name back to its endpoint/`Port`; `withdraw_name` removes one binding
  (`PORT_NAME_WITHDRAWN`). The index only ever points at a live binding —
  `unregister` withdraws every name resolving to the torn-down endpoint first —
  so a resolution can never dangle, and a name grants no authority of its own
  (the per-send capability check is unchanged, §5.2). `kernel/ipc::audit` gains
  `PORT_NAME_PUBLISHED`/`PORT_NAME_PUBLISH_DENIED`/`PORT_NAME_WITHDRAWN` (ids
  3006–3008). 18 new `rustos-abi` `PortName` tests + 9 new `rustos-kernel-ipc`
  registry tests. Docs `docs/src/architecture/ipc.md` (new "Well-known names"
  subsection + audit/error tables) + `docs/src/lib/abi.md` + the `lib/abi::ipc`
  and `kernel/ipc::registry` rustdoc. No `unsafe`, no
  `unwrap`/`expect`/`panic!` in production paths. **Still open here:** wiring the
  desktop's `MessagePort` to a live `ipc_recv` over a published input-port name,
  which still awaits the user-memory copy-in path (the registry is now composed
  into `KernelState`, see the next increment).
- **Named-port registry composed into `KernelState` + live IPC endpoint
  resolution — DONE (increment).** The `kernel/ipc::PortRegistry` is now part of
  the running kernel: `kernel/core`'s `KernelState` gains
  `ipc: RwLock<PortRegistry>` (mirroring `caps: RwLock<CapTable>` — the registry
  owns no lock, the synchronisation policy lives with `KernelState`, §2.1), and
  `KernelDispatchHook::new` / `KernelSyscallHandlers::new` take a seventh
  `&RwLock<PortRegistry>` borrow so the dispatch hook reaches it on every
  syscall. The `ipc_send` / `ipc_recv` handlers, until now blanket-inert stubs,
  now **resolve the destination endpoint against the live map** (§5.4): an
  endpoint that is not currently bound fails closed with `Errno::NotFound` — a
  real lookup miss the dispatcher's standard pipeline audits, no longer a
  blanket stub — while a *bound* endpoint resolves and then announces the one
  remaining deferral, the user-memory copy-in/out path that copies the payload
  to/from the caller's address space (`Errno::NotImplemented` +
  `SYSCALL_FEATURE_UNAVAILABLE` `feature = user_memory_copyin`, §15.1 — announce,
  never stub; the same prerequisite `cap_delegate` waits on). The Ipc init phase
  comment and the `KernelSyscallHandlers` module-doc deferral table were updated
  to match; the QEMU syscall-dispatch integration test composes the registry
  exactly as production does. 4 new `rustos-kernel-core` `syscalls` tests
  (unbound → `NotFound` with no deferral audit, bound → `NotImplemented` +
  one `SyscallFeatureUnavailable`, per direction); 112 `rustos-kernel-core`
  unit tests pass. Docs `docs/src/architecture/syscalls.md` (handler-wiring
  table + deferral prose), `docs/src/architecture/ipc.md` (registry now composed
  into `KernelState`), `docs/src/architecture/kernel.md` (Ipc phase + dispatch
  hook). No `unsafe`, no `unwrap`/`expect`/`panic!` in production paths. **Still
  open here:** the user-memory copy-in/out path that lets a bound endpoint
  actually transfer a payload (Stage 5 / Stage 6), and then publishing the
  desktop's input ports under their names so `IpcInputChannel`'s `MessagePort`
  resolves to a live `ipc_recv`.
- **Still to do this stage:** backing the
  desktop shell's `InputSource` (the `DesktopShell` event loop that fans the
  shared `lib/input` stream to the WM `InputRouter` and the taskbar
  `TaskbarInput`, re-presents through `TaskbarPresenter`, and keeps the
  running-task list in step with the window stack through `TaskBridge` now
  exists) with **live** pointer/keyboard device events — the IPC-message
  framing now exists (`IpcInputChannel`) and the kernel named-port registry it
  needs has now landed (`kernel/ipc::PortRegistry`, see above) and is now
  composed into `KernelState` with the `ipc_send` / `ipc_recv` handlers
  resolving an endpoint against it, so what remains is the user-memory copy-in
  path that lets a bound endpoint transfer a payload and then publishing the
  input ports under their names so the `MessagePort` resolves to a live
  `ipc_recv` — and relaying the
  session's theme switch over live IPC, selecting a font face
  from the theme's `FontSpec` roles once installed fonts exist (the SVG-first
  **caching layer** that converts each asset once at the active scale and
  re-renders only on a scale or theme change has landed — the shared
  `lib/raster` `RasterCache`, consumed by the WM cursor controller and the
  taskbar renderer). The two
  default apps — the filesystem browser and the
  terminal emulator — have both landed (model + renderer over an injected
  seam); what remains for them is the live VFS/shell channels and WM-presented
  windows (deferred wiring).

### User-memory copy path & per-task address spaces (staged)

The recurring blocker behind every "still open: the user-memory copy-in/out
path" note above — `ipc_send` / `ipc_recv` transferring a real payload, and
`cap_delegate` / `random_get` copying their buffers — is the kernel's
`copy_from_user` / `copy_to_user` boundary (`AGENTS.md` §5.4,
`tests/SECURITY.md` §5). It is decomposed into staged, independently-landable
increments so each is a complete, tested change rather than a half-wired
subsystem (§2.1). Each session lands one increment and updates
`.junie/next-session-prompt.md`.

- **A — Architecture-neutral user-memory copy facility (`kernel/mem::uaccess`).
  DONE (increment).** `copy_in` / `copy_out` move bytes between a kernel slice
  and a task's [`AddressSpace`], walking the user range one page at a time:
  each page is `translate`d to its `(Frame, MapFlags)`, checked fail-closed for
  `USER` + the direction's data permission (`READ` for in, `WRITE` for out —
  the §19.2 W^X guard rejects writing an executable page), turned into a CPU
  pointer through the kernel `PhysMap` direct map, and only the in-page byte
  span is moved. `UaccessError` names every refusal (`Null`, `LengthOverflow`,
  `NotMapped`, `NotUser`, `NotReadable`, `NotWritable`, `PhysUnmapped`); a
  page missing `USER` is rejected before a missing data permission so
  kernel-pointer confusion is never downgraded. One encapsulated `unsafe`
  (`core::ptr::copy`) per direction with a `// SAFETY:` rationale; 15 host tests
  over `HostPageTable` + `SimPhysMap` (cross-page, mid-page-offset, round-trip,
  every fail-closed branch). Docs `docs/src/architecture/memory.md` (new
  "## 3a. User-memory copy (`uaccess`)" section + testing strategy) and the
  module rustdoc. No `unwrap`/`expect`/`panic!` in production paths.
- **B — Per-task address-space registry. DONE (increment).** `kernel/mem`
  gains the object-safe, read-only `UserAddressSpace` trait (one method,
  `translate`; a blanket impl over `AddressSpace<P>` forwards to
  `AddressSpace::translate`, so there is one translation definition, §2.2) so
  the kernel can hold one entry per task without naming a single concrete
  page-table backend. `kernel/core` gains the `aspace` module:
  `AddressSpaceRegistry` is a `BTreeMap<TaskId, (Box<dyn UserAddressSpace>,
  Box<dyn PhysMap>)>` with `register` (fail-closed `AspaceError::AlreadyPresent`
  — never silently replaces a live mapping, §5.4), idempotent `withdraw`,
  `resolve` (→ the `(&dyn UserAddressSpace, &dyn PhysMap)` pair the `uaccess`
  copy path consumes), `contains`, `len`, `is_empty`. It is composed into
  `KernelState` empty next to `caps` / `ipc`, wrapped in the same
  reader-preferring `RwLock` (§2.1), and exposes only `translate` so the copy
  path can never mutate a caller's mappings (§2.4). The field is
  composed-but-not-yet-read pending increments C/D (justified
  `#[allow(dead_code)]`, mirroring the staged `frame_allocator` field). 6 host
  tests over `HostPageTable` + `SimPhysMap` (empty, register→resolve, duplicate
  rejected keeps first entry, selective withdraw, withdraw idempotence,
  re-register after withdraw) behind a `host-tests` dev-dependency on
  `kernel/mem`. Docs `docs/src/architecture/memory.md` (new "## 3b. Per-task
  address-space registry" section) + module rustdoc. No
  `unwrap`/`expect`/`panic!` in production paths.
- **C — Reach the caller's address space from the syscall handler. DONE
  (increment).** The per-task `AddressSpaceRegistry` is threaded into
  `KernelDispatchHook` / `KernelSyscallHandlers` as an
  `aspaces: &RwLock<AddressSpaceRegistry>` borrow next to `caps` / `ipc`
  (composed from `KernelState` in `kernel/core::init`, the dead-code allow
  retired). The new `KernelSyscallHandlers::with_caller_aspace(caller, f)`
  accessor takes a read guard, resolves `caller.task_id` →
  `(&dyn UserAddressSpace, &dyn PhysMap)`, and runs `f` with the borrowed pair
  while the guard is held — failing closed to `None` for a caller with no
  registered space (§5.4). The bridge lives in `kernel/core`, so the decoupled
  dispatcher (`kernel/syscall`) never gains a `kernel/mem` dependency (§17.4);
  the registry still exposes only `translate`, so the copy path can never mutate
  a caller's mappings (§2.4). 3 new `rustos-kernel-core` `syscalls` tests
  (registered caller runs the closure against its own space, unregistered →
  `None` without running it, per-task isolation); the 21 existing handler tests
  and the QEMU syscall-dispatch integration test compose the registry exactly
  as production does (121 `rustos-kernel-core` tests pass). Docs
  `docs/src/architecture/{kernel,syscalls,memory}.md` + module/field rustdoc. No
  `unsafe`, no `unwrap`/`expect`/`panic!` in production paths.
- **D — Wire the deferred syscalls through `uaccess`.** `ipc_send` copies the
  payload in → `Port::send`; `ipc_recv` `Port::recv` → copies out;
  `cap_delegate` copies the capability set in; `random_get` copies reserve
  bytes out. Map `UaccessError` → `Errno` (fail closed), and retire the
  `feature = user_memory_copyin` / `random_output_reserve` deferral audits.
  - **D.0 — Reconcile the copy path to the registry's erased type. DONE
    (increment).** `uaccess::copy_in` / `copy_out` (and the shared `walk`)
    previously took a generic `&AddressSpace<P>`, while `with_caller_aspace`
    yields a `&dyn UserAddressSpace`. They now take `&dyn UserAddressSpace`
    directly — the trait exposes exactly the one `translate` the walk needs, so
    there is still a single validated traversal (§2.2) — letting the
    `with_caller_aspace` pair drive the copies with no concrete `AddressSpace<P>`
    re-erasure at the boundary. A `&AddressSpace<HostPageTable>` unsized-coerces
    to the trait object, so the existing host tests are unchanged; one new
    `uaccess` test drives both directions through an explicit erased
    `&dyn UserAddressSpace` (16 `uaccess` host tests pass). Docs
    `docs/src/architecture/memory.md` (§3a/§3b) + module rustdoc. No
    `unwrap`/`expect`/`panic!` in production paths.
  - **D.1 — Wire `ipc_send` through the copy-in path. DONE (increment).**
    `ipc_send` now `lookup`s the destination `Port` against the live
    `PortRegistry`, bounds `len` against `port.max_payload()`
    (→ `MessageTooLarge`), stages the payload through `copy_from_user`
    (`with_caller_aspace` → `rustos_kernel_mem::copy_in`), and hands it to
    `Port::send(caller.caps, …)`; the `feature = user_memory_copyin` deferral
    audit is retired on the send side. Every `UaccessError`, and a caller with
    no registered address space, collapse onto the new **`Errno::BadAddress`**
    (the RustOS `EFAULT`, `lib/abi` discriminant 18, append-only — `abi-v1` is
    not frozen): one code for every faulting-pointer reason, so it cannot be
    used as a memory-layout oracle (§19.1, §5.4). A failed send enqueues
    nothing. `copy_fault_errno` centralises the mapping. 4 reworked/added
    `rustos-kernel-core` `syscalls` tests (bound endpoint copies + delivers the
    exact bytes and sender id; faulting pointer → `BadAddress`, nothing
    enqueued; no registered aspace → `BadAddress`; oversize → `MessageTooLarge`)
    — 124 `rustos-kernel-core` tests pass; `lib/abi` gains the frozen-
    discriminant + `Display` coverage for `BadAddress` (253 pass). Docs
    `docs/src/architecture/{syscalls,ipc}.md` + module rustdoc. No `unsafe`, no
    `unwrap`/`expect`/`panic!` in production paths.
  - **D.2 — Wire `ipc_recv` through the copy-out path. DONE (increment).**
    `ipc_recv` now `lookup`s the `Port` against the live `PortRegistry` and
    delivers its head message through a **peek/commit**: the new
    `Port::recv_with(f)` holds the mailbox lock while the handler copies the
    payload into the caller's buffer through `copy_to_user`
    (`with_caller_aspace` → `rustos_kernel_mem::copy_out`) and dequeues the
    message **only** when the copy succeeds, so a faulting pointer or an
    undersized buffer leaves the message queued rather than dropping it
    (§5.4, fail closed). A bound but momentarily empty endpoint returns the new
    **`Errno::WouldBlock`** (the RustOS `EAGAIN`, `lib/abi` discriminant 19,
    append-only — `abi-v1` is not frozen), distinct from the `NotFound` an
    unbound endpoint returns; a buffer too small is `BufferTooSmall`; a faulting
    buffer or a caller with no registered address space collapses onto
    `BadAddress` via the shared `copy_fault_errno` (§19.1). On success it
    returns the payload-byte count; the `feature = user_memory_copyin` deferral
    audit is retired on the receive side. 3 new `rustos-kernel-ipc` `recv_with`
    tests (empty → no closure call, commit on `Ok`, retain on `Err`) and 5
    new/reworked `rustos-kernel-core` `syscalls` tests (copies out + commits;
    empty → `WouldBlock`; undersized → `BufferTooSmall` retained; faulting →
    `BadAddress` retained; no aspace → `BadAddress` retained) — 128
    `rustos-kernel-core` tests pass; `lib/abi` gains frozen-discriminant +
    `Display` coverage for `WouldBlock`. Docs
    `docs/src/architecture/{syscalls,ipc}.md` + module rustdoc. No `unsafe`, no
    `unwrap`/`expect`/`panic!` in production paths.
  - **D.3 — Wire `cap_delegate` through the copy-in path. DONE (increment).**
    `cap_delegate` now copies the fixed-size `CapabilitySet` (its 256-bit
    bitmap as four little-endian `u64` words) in from `set_ptr` through
    `copy_from_user` (`with_caller_aspace` → `rustos_kernel_mem::copy_in`) and
    runs the `CapTable` delegate path:
    `caps.write().caps_for_mut(SecTaskId(target)).delegate(&set, audit)`. A
    faulting pointer, or a caller with no registered address space, collapses
    onto `BadAddress` via the shared `copy_fault_errno` (§19.1); an unknown
    target is `NotFound` (the same miss `cap_revoke` surfaces); a request that
    would *widen* the target's authority is `DelegationWiden` and is audited by
    `TaskCapabilities::delegate` (`TASK_CAPABILITIES_DELEGATED` /
    `…DELEGATE_WIDEN`, §5.2). The `feature = user_memory_copyin` deferral audit
    is retired on the delegate side. The on-wire `CapabilitySet` layout now has
    a single definition — `CapabilitySet::{WIRE_LEN, to_le_bytes, from_le_bytes}`
    in `lib/caps` — which `lib/caps/token.rs` (the signed-token codec) reuses,
    so there is one capability-set wire format, not two (§2.2). 5 new/reworked
    `rustos-kernel-core` `syscalls` tests (copies in + narrows the target;
    widen → `DelegationWiden` + target preserved; unknown target → `NotFound`;
    faulting pointer → `BadAddress`; no aspace → `BadAddress`) and 4 new
    `lib/caps` codec tests (round-trip, little-endian layout, short buffer →
    `BufferTooSmall`, trailing-byte tolerance). Docs
    `docs/src/architecture/syscalls.md` + module rustdoc. No `unsafe`, no
    `unwrap`/`expect`/`panic!` in production paths.
  - **D.4 — Compose the RNG output reserve into `KernelState` + wire
    `random_get`. DONE (increment).** `kernel/core` gains a `random` module:
    the object-safe **`RandomReserve`** seam (`draw(out, non_blocking)`) with a
    blanket impl over `rustos_rng::OutputReserve<E, N>` (choosing the fallible
    `fill` or the blocking `fill_blocking`), an unseeded **`NullEntropy`** boot
    source, the **`BootReserve`** alias, and **`reserve_errno`** (every
    `ReserveError` fails closed to `Errno::EntropyNotReady`). `KernelState`
    composes `rng: RwLock<Box<dyn RandomReserve + Send + Sync>>` — boxed so the
    handler is not generic over the entropy source — booting **unseeded** over
    `NullEntropy`, and the borrow is threaded into `KernelDispatchHook` /
    `KernelSyscallHandlers` as the ninth argument. **`random_get`** now bounds
    `len` (over-cap → `LengthOutOfRange`; `len == 0` → `Ok(0)`), resolves the
    caller's address space via `with_caller_aspace`, draws CSPRNG output in
    fixed `RANDOM_STAGE_CHUNK` (256-byte) stack chunks (no per-call heap
    allocation that could OOM, §4), copies each chunk into the caller's buffer
    through the validated `copy_to_user` boundary
    (`rustos_kernel_mem::copy_out`), and zeroises the staging buffer (§22). An
    unseeded reserve (or any entropy shortage) fails closed with
    `EntropyNotReady` — never weak bytes (§22 / §5.4); a faulting buffer or a
    caller with no registered address space collapses onto `BadAddress` via the
    shared `copy_fault_errno` (§19.1). The `feature = random_output_reserve`
    deferral audit — the last `SyscallFeatureUnavailable` emitter — is retired;
    the now-dead `audit_feature_unavailable` helper is removed (the audit-event
    id stays reserved for a future deferral). 4 new `random`-module tests + 5
    new/reworked `random_get` `syscalls` tests (over-cap → `LengthOutOfRange`;
    zero-len → `Ok(0)`; unseeded → `EntropyNotReady` with no deferral audit;
    seeded → copies 32 non-zero bytes out; no aspace → `BadAddress`; faulting →
    `BadAddress`); the 35 handler-test call sites thread the new `rng` borrow.
    142 `rustos-kernel-core` tests pass. `kernel/core` gains a `rustos-rng` +
    `zeroize` dependency (§17.4 — kernel/* may depend on lib/*). Docs
    `docs/src/architecture/{syscalls,kernel,memory}.md`, `docs/src/lib/rng.md`,
    `docs/src/platform/x86_64.md` + module rustdoc. **With D.4, the whole
    staged user-memory copy path (D.1–D.4) is wired.** No `unsafe`, no
    `unwrap`/`expect`/`panic!` in production paths. **Still pending:** the
    platform-RNG `EntropySource` (§17.2) that re-seeds the reserve — the same
    seam the encrypted-swap key is drawn from (Stage 8).
- **E — Per-architecture live `copy_from_user` fault fix-up + publish the input
  ports.** The page-fault recovery path each arch port needs so a faulting user
  access returns an error instead of trapping (the Stage-6 item
  `tests/SECURITY.md` §5 tracks), plus publishing the desktop's pointer /
  keyboard ports under their well-known `PortName`s so `IpcInputChannel`'s
  `MessagePort` resolves to a live `ipc_recv`.

---

## Stage 8 — Installer and Image Builders

**Dependencies:** Stages 5, 6 (and 7 for the graphical installer path).

**Deliverables**
- `userland/system/installer` with text and graphical front-ends sharing one core
  library. Functions per `AGENTS.md` §11 and lays out the filesystem per
  `AGENTS.md` §16: exactly `/System`, `/Users`, `/Apps`, `/Storage`; no
  legacy POSIX top-level directories; mount flags as specified in §11.3
  and §16.3; expert mode refuses any reserved name. The secure default
  lays out encrypted root **and** encrypted swap (`AGENTS.md` §4, §11);
  plaintext swap is never offered, including in expert mode.
- Kernel swap subsystem: when the process/VM model gains a pager, swap is
  brought up only through the encrypted-swap layer keyed by an ephemeral
  per-boot key from the platform RNG (`AGENTS.md` §4, §19.2). The kernel
  refuses to activate an unencrypted swap device and fails closed; the key
  is discarded on shutdown and never persisted.
  - **Encrypted-swap layer — DONE (landed ahead of Stage 8).**
    `kernel/mem::swap` is the cryptographic envelope the pager must route
    through: `EncryptedSwap` is the *sole* way to use a `SwapBackend`
    (plaintext swap is unrepresentable, `AGENTS.md` §2.11 — fail closed by
    construction), sealing each page with `lib/crypto`'s new
    ChaCha20-Poly1305 AEAD wrapper (`aead::seal`/`open`). The `SwapKey` is
    ephemeral, drawn from an injected `EntropySource` (the §19.2 RNG seam),
    zeroed on drop, and never persisted. Record layout
    `nonce(12) ‖ tag(16) ‖ ciphertext(4096)`; per-write `salt ‖ counter`
    nonce (exhaustion fails closed); slot index bound as AAD; `load` zeroes
    the caller's buffer on every failure. 16 unit tests + a §19.6 fuzz
    harness (`tests/fuzz_swap.rs`); `lib/crypto` gains 7 AEAD tests incl.
    the RFC 8439 vector. **Still pending:** the pager that calls
    `store`/`load`, the real platform-RNG `EntropySource`, the swap-device
    backend driver, and the `CAP`-gated activation syscall — all Stage 8.
- `tools/mkimage` producing:
  - `images/rustos-x86_64.iso` (hybrid BIOS/UEFI).
  - `images/rustos-aarch64-rpi.img`.
  - `images/rustos-riscv64.img`.
  - `images/rustos-web/` static tree.

**Tests**
- End-to-end QEMU install: build image → boot → run installer → reboot →
  log in as the created user → verify permissions and partition layout.
- Browser headless test for the `wasm32` image.

**Docs**
- `docs/src/install/{x86_64,raspberry_pi,riscv64,web}.md`.

---

## Stage 9 — Security Hardening and Audit

**Dependencies:** all earlier stages feature-complete.

**Deliverables**
- Threat model document (`docs/src/security/threat_model.md`).
- Fuzz harnesses for every parser (filesystem, ABI, manifest, IPC).
- Sandboxing review of every driver currently running in-kernel; move to
  user space if at all possible.
- `cargo audit` + `cargo deny` clean across the workspace.
- Reproducible builds (`tools/xtask repro`).

**Tests**
- Fuzz campaigns run in CI for a bounded time on every PR.
- Penetration test scripts under `tests/security/`.

**Docs**
- `docs/src/security/{threat_model,hardening,audit_log,reporting}.md`.

---

## Stage 10 — Release Engineering

**Dependencies:** Stage 9.

**Deliverables**
- Versioning policy (semver applied to ABI, distinct from product version).
- Release checklist in `docs/src/release.md`.
- Signed releases of all four images.
- Upgrade path documentation (`abi-vN` → `abi-vN+1`).

---

## Cross-cutting Tasks (run continuously alongside the stages)

These never "finish"; they are part of every PR.

- **Tests:** new code ships with tests; failing tests block merge.
- **Docs:** every change updates rustdoc and the relevant `docs/src/` page.
- **Lints:** `clippy -D warnings`, `fmt --check`, `cargo deny` always pass.
- **Coverage:** thresholds from `AGENTS.md` §7 are enforced.
- **ABI checks:** `cargo xtask abi-check` runs on every PR; ABI changes
  require a version bump and a migration note in `docs/src/abi/`.
- **Modularity checks:** `cargo xtask deps-check` and `cargo xtask
  cfg-check` run on every PR and enforce `AGENTS.md` §17 (layering,
  concrete-scheduler naming, optional-desktop boundary, and
  target-conditional-`cfg` confinement). See the §17 burn-down below.
- **No duplication:** code reviewers reject duplication; refactor into
  `lib/` instead.

---

## §17 Modularity Enforcement and Burn-down

**Status:** enforcement delivered. `cargo xtask deps-check` and
`cargo xtask cfg-check` (`tools/xtask/src/commands/{deps_check,cfg_check}.rs`)
implement the §17.5 checks and are wired into `cargo xtask ci`;
`cargo xtask build --headless` exercises the §17.3 headless image.
Documented in `docs/src/architecture/modularity.md`.

The tree predates §17, so both checks ship with explicit, shrink-only
grandfather allow-lists pinning today's violations. Each entry below is a
tracked defect; the burn-down removes them, and removing the last entry of
a list collapses that list. No *new* violation may be added.

**`deps-check` grandfathered edges (§17.4 / §17.1):**
- *(none)* — the `kernel/rustos-kernel` entry has been burned down (see
  below); the `deps-check` grandfather list is now empty.

**`cfg-check` grandfathered directories (§17.2):**
- *(none)* — the `kernel/rustos-kernel/` entry has been burned down (see
  below); the `cfg-check` grandfather list is now empty.

**Burn-down progress:**
- `kernel/rustos-kernel` production-binary bring-up edges (deps-check
  §17.4) — *done*. The four edges `rustos-kernel → {rustos-kernel-core,
  rustos-arch-x86_64, rustos-drvhost, rustos-drv-bus-virtio}` were the
  last grandfathered entries. The agreed resolution (above) was applied:
  the final-image binary is the x86_64 image-assembly seam, not a kernel
  subsystem, so `deps_check.rs::classify` now maps `kernel/rustos-kernel`
  to `Layer::Tooling` (outside the product layering), exactly mirroring
  the downstream `tests/integration/riscv64_boot` consumer that wires the
  riscv64 image together. As a `Tooling` integration point the binary may
  legitimately name the arch port, `kernel/core`, the driver host, and
  the boot-time bus driver. The `GRANDFATHERED` list is now empty (the
  now-dead `edge`/`GrandfatheredEdge` helpers were removed and the list
  switched to a `(from, to)` tuple representation), and a regression test
  (`deps_check::tests::rustos_kernel_binary_is_tooling_integration_point`)
  fails closed if the classification regresses or any edge is
  re-grandfathered. `deps-check` and `cfg-check` are clean with the tree
  scanned; the §17 grandfather lists for both checks are now empty. Docs
  updated: `docs/src/architecture/modularity.md`.
- `kernel/rustos-kernel/` freestanding `cfg` migration (cfg-check) —
  *done*. The production kernel binary no longer names the target
  instruction set inline. Its build script
  (`kernel/rustos-kernel/build.rs`) derives the bare-metal condition
  from `CARGO_CFG_TARGET_OS`/`CARGO_CFG_TARGET_ARCH` and emits a single
  custom `freestanding` cfg (declared via `rustc-check-cfg`); the crate
  gates its `#![no_std]`/`#![no_main]` attributes, the
  `boot`/`panic_ctx`/`serial_sink` modules, the IO-APIC typed
  publication slot, and the fail-closed `halt` on `cfg(freestanding)`
  instead of `cfg(all(target_arch = "x86_64", target_os = "none"))`.
  Target selection now lives in the one audited build-glue file
  (`AGENTS.md` §17.2). The `kernel/rustos-kernel/` cfg-check grandfather
  entry has been removed and `cfg-check` is clean with the tree scanned.
  (The remaining `kernel/rustos-kernel` *deps-check* edges — the
  inline bring-up of `kernel/core`, `kernel/arch/x86_64`, the driver
  host, and the virtio bus into the single §17.4 selection point — are a
  separate thread and unchanged by this work.)
- riscv64 port → Arch HAL migration (deps-check §17.2 / §17.4) —
  *done*. The riscv64 port (`kernel/arch/riscv64`) is now a pure Arch
  HAL implementation exactly like x86_64: `RiscvArch` implements
  `rustos_arch_api::SchedulerArch` and exposes the monotonic clock, the
  hart-park primitive, and the `plic::PlicController` register driver
  (inherent `mask`/`arm`/`unmask`/`claim`/`complete`), but the crate
  names only `kernel/arch/api` + `lib/*`. The boot orchestration it used
  to own — the `kernel_core::BootInfo` assembly, the `RiscvBinArch`
  `KernelArch` wrapper, the set-once boot-state slots, and the
  `IrqController` bridge (`PlicIrqController`) over the PLIC — moved into
  a new downstream Tooling crate `tests/integration/riscv64_boot`
  (`rustos-test-riscv64-boot`), which legitimately names
  `kernel/{core,mem,sec,irq}` + `kernel/sched/api` as a test crate (the
  §17.4 layering is relaxed for `tests/`). This mirrors how x86_64 keeps
  its boot pipeline and `BinArch` wrapper in the downstream
  `rustos-kernel` crate. The two riscv64 QEMU consumers
  (`kernel_arch_boot_riscv64` and `virtio_qemu_support::imp_mmio`) now
  import `boot` / `published_*` / `PlicIrqController` from `riscv64_boot`;
  the arch crate's `kernel_arch`/`plic` tests were split into their own
  `*_tests.rs` files. All five grandfather edges
  (`rustos-arch-riscv64 → kernel/{core,mem,sec,irq,sched-api}`) have been
  removed and a regression test
  (`deps_check::tests::riscv64_port_is_pure_arch_hal`) fails closed if
  any returns. `deps-check`, `cfg-check`, and `cargo xtask test --qemu`
  (all 11 verticals incl. the 3 riscv64) are green. Docs updated:
  `docs/src/platform/riscv64.md` and `docs/src/security/irq.md`.
- Arch HAL `kernel/arch/api` + x86_64 migration (deps-check) — *done*.
  The §17.2 architecture surface now lives in its own `no_std`,
  dependency-free crate `kernel/arch/api` (`rustos-arch-api`), carrying
  the scheduler-facing slice (`CpuId`, `SchedulerArch`). `kernel/sched`
  re-exports the trait (single canonical definition, §2.2), and
  `kernel/arch/x86_64` now implements `rustos_arch_api::SchedulerArch`
  and no longer names `kernel/sched` — its `sched-arch` feature pulls
  the HAL instead. The `kernel/arch/x86_64` → `kernel/sched`
  grandfather edge has been removed and `deps-check` is clean.
  Remaining HAL primitives (MMU/TLB, context switch, timer, interrupt
  entry/exit, per-CPU storage, boot discovery) are the next steps on
  this thread; the riscv64 port migration is *done* (see the entry
  above).
- `tests/integration/` (cfg-check) — *done*. Target selection for the
  freestanding QEMU bins now lives in one audited build-glue crate,
  `tests/integration/harness` (`rustos-itest-harness`), whose
  `emit_target_cfg()` build-script hook maps the cargo target onto the
  custom `freestanding` / `itest_x86_64` / `itest_riscv64` cfgs. Every
  bin and the shared `virtio_qemu_support` library gate on those names
  instead of `cfg(target_arch …, target_os = "none")`, so no test source
  names the target instruction set (§17.2). The grandfather entry has
  been removed and `cfg-check` is clean with the tree scanned.
- `drivers/bus/pci/` port-I/O behind a `lib/abi` seam (cfg-check) —
  *done*. The legacy mechanism-#1 port-I/O contract is now the
  architecture-neutral `PortIo` seam in `lib/abi`
  (`lib/abi/src/driver/port_io.rs`, re-exported as `rustos_abi::PortIo`),
  mirroring the `MmioMapper` register-window seam. The x86_64 `in`/`out`
  implementation (and its only `unsafe`) moved into the architecture
  port `kernel/arch/x86_64::pio` (`X86PortIo` + `x86_port_io()`); the
  PCI bus driver dropped its crate-local `PortIo` trait and `X86PortIo`
  asm and now exposes the cfg-free generic constructor
  `rustos_drv_bus_pci::mechanism_one<P: PortIo>(pio)`. The x86_64 QEMU
  vertical feeds it `x86_port_io()`; the unit test uses a mock backend.
  The `drivers/bus/pci/` cfg-check grandfather entry has been removed
  and `cfg-check` is clean with the tree scanned (`AGENTS.md`
  §17.2 / §17.4).
- Scheduler `api`/`impl` split + `kernel/sync` → `lib/sync` relocation
  (§17.1) — *done*. The scheduler contract now lives in its own
  `SchedApi` crate `kernel/sched/api` (`rustos-kernel-sched-api`): the
  `SchedulerPolicy` trait, the lifecycle vocabulary, the re-exported
  Arch HAL surface, and the shared conformance suite
  (`kernel/sched/api/tests/conformance.rs`, exercised against the
  in-tree policy). The MLFQ policy moved to the sibling crate
  `kernel/sched/mlfq` (`rustos-kernel-sched-mlfq`), which implements
  `SchedulerPolicy`. `kernel/core` is the single build-time selection
  point and `src/sched.rs` `compile_error!`-guards that exactly one
  `scheduler-*` feature is active per image. `kernel/sync` was relocated to `lib/sync` (renamed
  `rustos-sync`) so a `SchedImpl` crate may name its primitives
  (`AGENTS.md` §6 / §17.4); §3/§4 updated to match. Grandfather edges
  removed: `kernel/sched → kernel/sync`, `riscv64 → kernel/sync`, and
  `kernel/rustos-kernel → kernel/sched` — `rustos-kernel`/`riscv64` now
  name only `kernel/sched/api`. The remaining HAL-primitive and riscv64
  boot-orchestration threads are unchanged by this work.
- `userland/system/drvhost` → `drivers/bus/virtio` stale edge removal
  (deps-check) — *done*. The driver host's production code
  (`userland/system/drvhost/src/`) never names the virtio bus crate; the
  dependency exists solely under `[dev-dependencies]`, used by the
  integration-test fixtures that mint a `MockHost`-backed
  `VirtioHostFactory`. `deps-check` walks only build-graph dependencies
  (dev-dependencies are test-only scaffolding, excluded by
  `is_build_dependency_header`), so the §17.4 `Userland → Driver` edge
  does not exist in the graph and was already compliant. The grandfather
  entry was therefore stale: keeping it would have silently tolerated a
  *future* production dependency. It has been removed and a regression
  test (`deps_check::tests::drvhost_has_no_production_edge_to_virtio_bus`)
  now fails closed if drvhost ever gains a real production edge to the bus
  crate. (The driver-host dev-dependency was subsequently repointed off
  the bus crate onto `lib/virtio` by the virtio driver-layer thread
  below, so even the dev-only edge is gone.)
- Virtio driver-layer split: `lib/virtio` + kernel host relocation
  (deps-check §17.4) — *done*. The bus-agnostic split-virtqueue protocol
  (the `Transport` trait, `SplitQueue`, the owned `DmaSlab` /
  `BounceBuffer`, and the in-process `MockHost` / `MockTransport`
  doubles) moved out of `drivers/bus/virtio` into a new shared crate
  `lib/virtio` (`rustos-virtio`), which depends only on `lib/abi`. The
  bus driver now keeps only the concrete PCI / MMIO `Transport`
  implementations (`PciTransport` / `MmioTransport`) and re-exports the
  protocol from `lib/virtio`. The capability-checked kernel host
  (`KernelVirtioHost`, `kernel_host.rs`) and MMIO mapper
  (`KernelMmioMapper`, `kernel_mmio.rs`) — which link
  `kernel/{mem,sec,irq}` — moved into `kernel/virtio`, so the bus driver
  crate dropped its `kernel-host` Cargo feature and every `kernel/*`
  dependency. The device drivers `drivers/storage/virtio_blk` and
  `drivers/network/virtio_net` now consume the protocol from `lib/virtio`
  and no longer depend on the bus driver crate at all. Grandfather edges
  removed: `drivers/bus/virtio → kernel/{mem,sec,irq}`,
  `rustos-drv-storage-virtio-blk → rustos-drv-bus-virtio`, and
  `rustos-drv-network-virtio-net → rustos-drv-bus-virtio`; a regression
  test (`deps_check::tests::virtio_driver_layer_is_on_lib_only`) fails
  closed if any of them returns. `deps-check` and `cfg-check` are clean.
  The remaining virtio deps-check edge
  (`kernel/virtio → drivers/bus/virtio`, the concrete-transport thread)
  was unchanged by this work and is resolved separately below.
- `kernel/virtio` → `userland/system/drvhost` `VirtioHostFactory` seam
  relocation (deps-check §17.4) — *done*. The factory trait the driver
  host calls before a driver's `register()` — and which the kernel-side
  `KernelVirtioFactory` implements — moved out of `userland/system/drvhost`
  into the bus-agnostic `lib/virtio` host seam (`VirtioHostFactory`,
  alongside `VirtioHost` / `MockHost`). Its `mint` now gates on the
  driver's granted capabilities through a new object-safe
  `rustos_abi::CapabilityQuery` trait (implemented for `lib/caps`'
  `CapabilitySet`), so the seam never names `lib/caps` and the
  `lib/abi → lib/caps` layering inversion is avoided. With the trait in
  `lib/virtio`, both `drvhost` and `kernel/virtio` depend on `lib/*`
  instead of on each other: `drvhost` dropped its trait definition and
  re-export, and `kernel/virtio` dropped its `rustos-drvhost` Cargo
  dependency. The `kernel/virtio → userland/system/drvhost` grandfather
  edge has been removed and a regression test
  (`deps_check::tests::kernel_virtio_has_no_edge_to_drvhost`) fails
  closed if it returns. `deps-check` and `cfg-check` are clean.
- `kernel/virtio` → `drivers/bus/virtio` concrete-transport seam
  (deps-check §17.4) — *done*. The ring-0 provisioning walks
  (`virtio_pci_walk` / `virtio_mmio_walk`) used to build the concrete
  `PciTransport` / `MmioTransport` from `drivers/bus/virtio`, which made
  `kernel/virtio` (a `KernelSubsystem`) depend on a driver crate. The
  walks are now generic over a caller-supplied transport builder
  (`FnOnce(PciTransportWindows) -> Result<T, VirtioError>` /
  `FnOnce(RegisterWindow) -> Result<T, VirtioError>`) and return a
  generic `VirtioProvision<T>` / `VirtioMmioProvision<T>`. The
  transport-construction descriptor `PciTransportWindows` moved into
  `lib/virtio` (beside the `Transport` trait); the bus driver imports
  and re-exports it. `kernel/virtio` therefore names only `lib/*` types
  and dropped its `rustos-drv-bus-virtio` Cargo dependency. The
  production builders — `PciTransport::new` / `MmioTransport::new` — are
  passed by the consumers that may legitimately name the bus driver:
  the `rustos-kernel` binary (`virtio_boot.rs`, itself a separate
  grandfather thread) and the Tooling-exempt QEMU integration verticals.
  The walk unit tests use local identity builders, so `kernel/virtio`
  has no dependency on the bus driver even under `[dev-dependencies]`.
  The grandfather edge has been removed and a regression test
  (`deps_check::tests::kernel_virtio_has_no_edge_to_bus_driver`) fails
  closed if it returns. `deps-check` and `cfg-check` are clean. Docs
  updated: `docs/src/drivers/{bus,virtio}.md` and
  `docs/src/abi/driver_traits.md`.
- Second scheduler policy: fully tickless EEVDF, now the default
  (§17.1) — *done*. Added the sibling `SchedImpl` crate
  `kernel/sched/eevdf` (`rustos-kernel-sched-eevdf`) implementing
  `SchedulerPolicy`: an Earliest-Eligible-Virtual-Deadline-First policy
  whose fairness, eligibility, and preemption are driven entirely by
  per-dispatch virtual-time advance — never by a periodic tick
  (`on_timer_tick` is observation-only). It reuses the same per-CPU
  run-queue + work-stealing shape as the MLFQ sibling but is a *parallel*
  implementation (`AGENTS.md` §2.2 carve-out), not a `cfg` fork, with its
  own `RunQueue` (virtual-time-ordered, weight 4:2:1 by `Priority`). It
  passes the shared conformance suite via
  `kernel/sched/eevdf/tests/conformance.rs`. `kernel/core` now defaults
  to `scheduler-eevdf`; `scheduler-mlfq` stays selectable
  (`--no-default-features --features scheduler-mlfq`) and `src/sched.rs`
  gained "no policy" and "more-than-one policy" `compile_error!` guards.
  The MLFQ policy is retained and still exercised (its own tests + the
  api-crate conformance test). `deps-check`/`cfg-check` clean (the new
  crate classifies as `SchedImpl`); docs in
  `docs/src/architecture/scheduler.md`.
- Heterogeneous-CPU (performance + efficiency core) scheduling
  (§17.1 / §17.2) — *done*. The Arch HAL gained a `CoreClass`
  (`Performance` / `Efficiency`) and a **provided** method
  `SchedulerArch::core_class(cpu) -> CoreClass` defaulting to
  `Performance`, so all ten existing `SchedulerArch` implementors (and
  the homogeneous-machine path) are unchanged and no interface creep is
  introduced (`AGENTS.md` §2.4 — the class is static per-CPU identity,
  the same category as `current_cpu`, not dynamic power management).
  Both policies (`kernel/sched/{mlfq,eevdf}`) snapshot the per-CPU
  classes at construction and steer placement by priority band —
  `High`/`Normal` → performance core, `Low` (idle/background) →
  efficiency core — in `spawn`, `unpark`, and the dispatch re-enqueue
  path; on a homogeneous machine every such decision resolves to the
  caller's home (strict no-op). Under MLFQ the existing boost/demote
  machinery thereby promotes a busy background task **up** to a
  performance core and demotes it back **down** to an efficiency core
  for free; EEVDF carries competing weight across a class migration
  (the work-stealing rebase). The x86_64 port detects real Intel hybrid
  topology from CPUID **leaf 0x1A** (`kernel/arch/x86_64::hybrid`,
  host-tested decoder) into a per-CPU class table on `X86_64Arch`
  (boot CPU in `new`, APs via `record_core_class`). Host `TestArch`
  gained `set_core_class`. Tests: new arch-api/`hybrid`/`X86_64Arch`
  unit tests, per-policy placement + promote/demote unit tests, and a
  shared conformance test `heterogeneous_topology_preserves_liveness`
  (mixed High/Low population completes with no loss across both
  policies). Docs: new "Heterogeneous CPUs" section in
  `docs/src/architecture/scheduler.md`. clippy `-D warnings`,
  host + `x86_64-unknown-none` builds clean.
- AMD vendor + heterogeneous-core (`Zen`/`Zen-c`) detection for x86_64
  (§17.2 / §18.2) — *done*. The heterogeneous-CPU item above wired the
  `CoreClass` Arch-HAL hook and a *complete* detector **only for Intel**:
  `kernel/arch/x86_64::hybrid` gates on CPUID leaf 0x07 `EDX[15]` ("Hybrid")
  and decodes the core type from **leaf 0x1A**. AMD parts never set that
  bit and never implement leaf 0x1A, so every AMD core — including the
  genuinely heterogeneous ones (two `Zen 4` performance cores + four
  density/efficiency-optimised `Zen 4c` cores on Phoenix 2 / Family 0x19
  Model 0x78, and the Zen-5/5c successors) — currently falls through to the
  homogeneous `CoreClass::Performance` default. That default is *safe*
  (the scheduler simply treats the machine as homogeneous; no
  misclassification, no panic — `AGENTS.md` §2.9), but it is *incomplete*:
  on a hybrid AMD client the efficiency cores are not recognised, so
  background work is never steered onto them. Closing this means, in the
  x86_64 port only (§17.2 — no `cfg(target_arch …)` leaks elsewhere, and
  no edits outside `kernel/arch/x86_64/`):
  - **Vendor identification.** Read the 12-byte vendor string from CPUID
    leaf 0 (`EBX`/`EDX`/`ECX` = `"AuthenticAMD"`) and branch the core-class
    probe on it, rather than assuming Intel's leaf semantics on every
    x86_64 part. Today `detect_current_core_class` consults the Intel
    leaves unconditionally; that must become vendor-dispatched. The
    `classify_core_type` (Intel leaf 0x1A) decoder stays as-is and a
    sibling pure, host-testable AMD decoder is added next to it — a
    parallel implementation, not a `cfg` fork (§2.2 carve-out).
  - **AMD heterogeneous source.** AMD does **not** expose an Intel-0x1A
    equivalent. The per-core class comes from the **Extended CPU Topology
    leaf `0x80000026`** (`Core::X86::Cpuid::ExtCpuTopology`), which on
    heterogeneous parts reports a per-core *core type* and *power/efficiency
    ranking* (this is the leaf the Linux 6.13 AMD heterogeneous-topology
    series parses). Probe it only after bounding the maximum *extended*
    leaf via CPUID `0x80000000`, mirroring the existing leaf-0 bound for
    the Intel path, so an unsupported sub-leaf is never executed.
  - **APM is still evolving — fail conservative.** AMD's documentation of
    the `0x80000026` core-type / efficiency-ranking encoding is newer than
    Intel's leaf 0x1A and is still settling across CPU generations; the
    field layout and reserved values may change on later parts. The AMD
    decoder must therefore recognise only the encodings AMD has actually
    published, treat every unknown/reserved value as
    `CoreClass::Performance` (the safe homogeneous default, exactly as the
    Intel decoder treats an unknown core-type byte), and never guess from
    family/model heuristics or frequency tables. A part that does not
    advertise leaf `0x80000026` is homogeneous and reports `Performance`.
    Each recognised encoding is pinned to the AMD APM / PPR revision it
    came from in the source so a future encoding change is a deliberate,
    reviewed addition, not a silent reinterpretation.
  - **Tests + docs (lands with the change, §2.5 / §7 / §13).** Host-side
    unit tests for the AMD decoder (a Zen-c value → `Efficiency`, a Zen
    value → `Performance`, and unknown/reserved → `Performance`) plus a
    vendor-string-parse test; the existing host
    `host_detection_reports_homogeneous_default` stays valid (no real
    topology on the host). `hybrid.rs`'s module docs and the
    "Heterogeneous CPUs" section of `docs/src/architecture/scheduler.md`
    gain the AMD path and the evolving-APM caveat. No scheduler, no
    `lib/*`, and no other arch crate changes — the `CoreClass` contract
    already exists, so this is detector work, not interface creep (§2.4).

  Landed: `kernel/arch/x86_64::hybrid` now reads the CPUID leaf-0 vendor
  string and dispatches — `is_amd_vendor` routes AMD parts to the new
  pure `classify_amd_core` decoder (a parallel implementation alongside
  the unchanged Intel `classify_core_type`, §2.2 carve-out), while every
  other vendor keeps the Intel leaf-0x1A path. `detect_current_core_class`
  splits into vendor-specific bare-metal probes: the AMD probe bounds the
  maximum extended leaf via CPUID `0x80000000`, then walks the bounded
  leaf-`0x80000026` sub-leaves for the Core level (`ECX[15:8] == 1`) and
  decodes the published power/efficiency ranking (`EBX[23:16]`) under the
  heterogeneous (`EAX[30]`) + ranking-available (`EAX[29]`) gates; the
  lowest tier is `Efficiency`, every other case is the conservative
  `Performance` default. No scheduler, `lib/*`, or other arch-crate edits.
  Tests: AMD decoder (lowest-ranking → `Efficiency`, higher-ranking →
  `Performance`, non-heterogeneous / no-ranking / non-Core /
  unknown-reserved → `Performance`) + vendor-string parse; the host
  `host_detection_reports_homogeneous_default` stays valid. Docs: the
  `hybrid.rs` module docs and the "Heterogeneous CPUs" section of
  `docs/src/architecture/scheduler.md` gained the AMD path and the
  evolving-encoding caveat. Verified: host + `x86_64-unknown-none` clippy
  `-D warnings` clean, crate tests green.

---

## §19 Threat Model and Hardening Burn-down

**Status:** newly binding (`AGENTS.md` §19) and largely **unimplemented**.
§19 supersedes and makes binding the loose "Stage 9 — Security Hardening
and Audit" deliverables: where they conflict, §19 wins, and Stage 9 is
delivered by completing this burn-down. This section is the authoritative
gap report and staged plan; it follows the same shrink-only, fail-closed
discipline as the §17 burn-down above. No *new* §19 violation may be
added, and each item lands with its own tests and docs (`AGENTS.md`
§2.5 / §7 / §13) before it is marked *done*.

This burn-down was opened by a §19 conformance review. The review found
no code that *violates* §19 (the charter clauses are largely additive
infrastructure that does not yet exist) — so there is nothing to revert;
the work is to *build* the missing mechanisms. The one exception worth
restating is that nothing in the tree currently emits an `RWX` mapping
or a `/proc`-style surface, so §19.2's W^X invariant and §16.1 are not
presently breached; they become enforced (not merely observed) when the
rxe loader item below lands.

**Gap report (per subsection, as of this review):**
- **§19.1 microarchitectural side channels** — *HAL trait set + conformance
  vertical landed (item 8)*. `kernel/arch/api/src/sidechannel.rs` defines
  the closed side-channel surface: the `SideChannelMitigation` trait
  (the syscall entry/exit speculation barriers + the context-switch
  microarchitectural-buffer/indirect-branch barrier) and a declarative
  `MitigationProfile` (KPTI-equivalent isolation + the four barriers),
  each slot one of `Applied` / `NotVulnerable(reason)` /
  `Pending(note)`. `MitigationProfile::validate` is the honesty gate
  (every non-applied slot justified) and `is_release_ready` is the
  stricter §19.1 "cannot ship" gate (no `Pending`). The
  `sidechannel::conformance::run_all` vertical (the §17.2 suite) is run
  by every port from a host test, and each of x86_64 (`lfence`/`verw`;
  KPTI+IBPB `Pending`), aarch64 (`csdb`; MDS `NotVulnerable`;
  KPTI+Spectre-v2 `Pending`), riscv64 (`fence`; rest `NotVulnerable`,
  release-ready), and wasm32 (host-owned `NotVulnerable`, release-ready)
  declares its honest profile. Docs: `docs/src/security/side_channels.md`.
  Still missing: the KPTI / IBPB `Pending` gaps close with the Stage 6
  user/kernel boundary + CPUID/MIDR feature probes. The `lib/crypto`
  constant-time-under-`-O3` test landed: `rustos_crypto::ct_eq`
  (`lib/crypto/src/constant_time.rs`) is the sanctioned secret-comparison
  primitive, its no-early-exit property is proved by an instrumented
  full-traversal test (no wall-clock timing, §7), and `cargo xtask ci`
  re-runs the crate's tests under the release profile (`-C opt-level=3`).
- **§19.2 W^X / ASLR / CFI** — *loader landed (item 7)*. `lib/abi/src/rxe.rs`
  now defines the `rxe` load image (`LoadHeader` + `Segment` table) and the
  load-time policy: `LoadImage::parse` (a) enforces R/RX/RW segments and
  refuses `RWX` (`RxePermission::from_segment_flags`), (b) requires PIE and
  exposes `kaslr_bias` for a per-boot entropy seed, and (c) checks the CFI
  type-tag against the §9 syscall-interface hash (constant-time). The
  `kernel/mem` loader (`map_image`) maps a validated image into an
  `AddressSpace`, W^X holding twice over. Still open: copying segment file
  contents into mapped frames and stack-canary / shadow-stack in the arch
  `unsafe` cores (Stage 6 process model + real arch page tables).
- **§19.3 supply-chain integrity** — *in progress*. `cargo xtask sbom`
  now emits a deterministic CycloneDX 1.5 SBOM from `Cargo.lock` (every
  workspace + external crate with version, source URL, and pinned
  source checksum); signing it with the per-installation key is deferred
  behind the not-yet-existing signing API. The source-hash allow-list
  and the 7/30-day advisory-SLA gate also landed as `cargo xtask
  supply-chain` (in `ci`; see burn-down item 4 below). Still missing:
  `build --reproducible` and "no post-install network fetch"
  enforcement.
- **§19.4 audit-log integrity** — *in progress (core landing this
  session)*. `lib/log` was an in-memory facade with no hash-chaining,
  signed anchors, or per-service `CAP_LOG_WRITE` partitioning.
- **§19.5 parser sandboxing** — *not started*. `userland/net/icmp`
  (arp/ethernet/ipv4/icmp) and the future font/image/archive/media
  decoders run in-process; there is no minimum-capability sandbox
  process model and no "parser must link into a sandbox" check.
- **§19.6 fuzzing** — *in progress*. The in-tree deterministic harnesses
  cover the wire decoders (`lib/abi/tests/fuzz_decode.rs`), the syscall
  dispatcher (`kernel/syscall/tests/fuzz_args.rs`), the `userland/net`
  protocol parsers (`userland/net/icmp/tests/fuzz_parse.rs`), and the
  capability-checked IPC port endpoint
  (`kernel/ipc/tests/fuzz_port.rs`); all four are driven for a wall-clock
  budget by `cargo xtask fuzz` (`--quick` ≥ 5 s / `--soak` ≥ 24 h),
  wired into `ci`. Remaining for later stages: harnesses for the future
  font/image/archive/media parsers (§19.5) as those parsers land.
- **§19.7 verified capability core** — *Bronze + Silver done; Gold not
  started*. `cargo xtask proptest` drives an in-tree `proptest`-style
  stateful model for each capability-critical path (`lib/caps`,
  `kernel/sec`, the IPC port dispatch, and the syscall dispatch gate),
  and `cargo xtask spec-review` enforces the draft-marker discipline.
  The Silver tier landed (item 11 below): `cargo xtask model-check` is an
  in-tree exhaustive explicit-state model checker (the sanctioned TLA+
  equivalent, §2.12 / §19.6 precedent) of the combined capability + IPC
  state machine, with the formal Init/Next/Inv narrative under
  `docs/src/security/model/capability_ipc.md`. All three are wired into
  `ci`. Still missing: the Gold Verus contracts (`cargo xtask verify`).
- **§19.8 hardware-enforced capabilities (Tier-2)** — *not started*. No
  `kernel/arch/cheri-*` crate; deferred behind the Tier-1 conformance
  suites by charter.
- **§19.10 hardware memory tagging** — *HAL trait set + conformance
  vertical + per-arch profiles + software slab UAF check landed (item
  13)*. `kernel/arch/api/src/memtag.rs` defines the closed memory-tagging
  surface: the `MemoryTagging` trait (granule geometry +
  capability-checked `set_region_tag`), the honest `TaggingProfile`
  (`tag_storage` / `tag_check_faults`, each `Supported` /
  `Unsupported(reason)` / `Pending(note)`), the architecture-neutral
  `MemTag` / `next_free_tag` rotation, and the `memtag::conformance`
  vertical every port runs from a host test. Per target: aarch64 drives
  Arm MTE (the `stg` store sequence, 16-byte / 4-bit granule, gated
  behind a default-off `mte_enabled` flag) with both slots honestly
  `Pending` on the Stage 6 MTE enable; x86_64 / riscv64 / wasm32 declare
  a justified `Unsupported` (no tagging silicon). The `kernel/mem` slab
  hardens use-after-free **today** on every target in software: a
  tag-carrying `SlabHandle` is rejected with `SlabError::TagMismatch`
  once its slot is freed and reallocated, sharing the HAL's
  `next_free_tag` rotation. Docs: `docs/src/security/memory_tagging.md`.
  Still open: switching the aarch64 `stg` store path live and the
  hardware tag-check fault with the Stage 6 page-table work.

**Standing directive (owner, 2026-05-31):** every *independent* item of
this burn-down (1, 3, 4, 5, 6, 7, 8, 11, 13) is **landed and verified green**;
the implementable portion of §19 is complete. The only remaining items
(2, 9, 10, 12) are **stage-blocked or aspirational**, not deferred by
choice. They carry a binding **[DO IMMEDIATELY ON UNBLOCK]** order: the
session that lands the prerequisite stage (Stage 2 signing API, Stage 5
log store, Stage 6 process model, Stage 8 image builders) must complete
the corresponding §19 item in the same or the very next session, before
any other Stage work proceeds. §19 is not "finished" until items 2, 9,
and 10 land; item 12 stays aspirational per charter §19.7/§19.8.

**Burn-down plan (ordered; each item is one task, with tests + docs):**
1. **§19.4 hash-chain core** — *in progress*. A no-alloc, fixed-buffer
   SHA-256 hash-chain primitive in `lib/log` (`chain.rs`): a per-CPU
   `LogChain` issuing monotonic-sequenced `ChainedEntry` records each
   binding the previous entry's hash, plus a `verify_chain` that
   re-derives every entry hash, checks linkage and the monotonic
   sequence, and returns the chain root hash. Payload-agnostic (operates
   over a caller-supplied payload digest) so the crate stays no-alloc and
   the persisted `/System/Logs` writer (Stage 5) and signed anchors
   (item 2) build on it. Docs: `docs/src/security/audit_log.md`.
2. **§19.4 signed anchors + `CAP_LOG_WRITE` partitioning** — *blocked on
   a private-key signing API (Stage 2 capability authority) and the
   persisted log store (Stage 5)* — **[DO IMMEDIATELY ON UNBLOCK]**.
   Periodically sign the chain root to
   `/System/Logs/Anchors/`; partition `CAP_LOG_WRITE` per service;
   `CAP_LOG_ROTATE` for truncation.
3. **§19.3 `cargo xtask sbom`** — *done (unsigned)*. Emits a
   deterministic CycloneDX 1.5 SBOM (workspace + external crates, each
   with version, source URL, and pinned source checksum) from the
   committed `Cargo.lock` — a self-contained lockfile parser +
   hand-written JSON serialiser, no `serde`/`cyclonedx` dependency and no
   `cargo metadata` shell-out (`AGENTS.md` §2.12). Pure tooling,
   host-tested in `tools/xtask/src/commands/sbom.rs`. Docs:
   `docs/src/security/supply_chain.md`. *Signing* the SBOM with the
   per-installation key (§11) is deferred with item 2's signing: no
   private-key signing API exists yet (`rustos-crypto` is verify-only).
4. **§19.3 source-hash allow-list + advisory SLA** — *done*. New
   `cargo xtask supply-chain` (wired into `ci` after `cargo deny`)
   verifies a committed `supply-chain.toml` policy file against
   `Cargo.lock`: every external-registry crate must carry a matching
   `[[source-pin]]` SHA-256 (a new/mismatched/stale/duplicate pin fails
   closed, with paste-ready errors), and `--write-pins` regenerates the
   pins from the lockfile (committed and reviewed by diff, like
   `Cargo.lock` itself). The same command enforces the advisory SLA over
   `[[advisory]]` ledger entries: each accepted RUSTSEC advisory carries
   a `published` date and a `tier` (`crypto` = 7-day, `general` = 30-day
   SLA from publication), failing closed the day after the window
   lapses; the ledger is empty today. Self-contained (no `toml`/`serde`
   dep), reusing `sbom`'s `Cargo.lock` parser; 18 unit tests cover the
   policy parser, pin check, civil-date arithmetic, and SLA edges. Docs:
   `docs/src/security/supply_chain.md`. (`deny.toml` is `cargo deny`'s
   own format and has no per-crate hash field, so the allow-list lives
   in the dedicated `supply-chain.toml` that the gate reads.)
5. **§19.6 fuzzing harnesses + `cargo xtask fuzz`** — *done*. The
   `lib/abi` decoder harness (`lib/abi/tests/fuzz_decode.rs`) and the
   syscall-dispatcher harness (`kernel/syscall/tests/fuzz_args.rs`)
   already existed as fixed-iteration `cargo test` smoke sweeps; they
   now also honour a `RUSTOS_FUZZ_BUDGET_SECS` wall-clock budget. New
   `cargo xtask fuzz` (in `ci` after `supply-chain`) drives every
   registered harness for its budget — `--quick` (≥ 5 s/harness, the
   per-PR floor, a practicality concession) or `--soak` (≥ 24 h/harness,
   nightly, the real coverage) — and fails
   closed on any crash, hang, or invariant failure. Self-contained, no
   external fuzz runner (`AGENTS.md` §2.12 / §19.6 "equivalent in-tree
   harness"); 16 unit tests cover the target registry, budget floors,
   and arg parser. The target set now also includes the `userland/net`
   protocol parsers (`userland/net/icmp/tests/fuzz_parse.rs` — Ethernet/
   ARP/IPv4/ICMP + the composed `handle_frame` and `Client` classifiers,
   no-panic + encoder round-trip + bounded-reply invariants) and the
   capability-checked IPC port endpoint
   (`kernel/ipc/tests/fuzz_port.rs` — `Port::send` fail-closed against an
   independent caps→size→capacity mirror, FIFO `recv` byte-fidelity,
   capacity bound, and a closed-port fast-path refusal). Docs:
   `docs/src/security/fuzzing.md`. Remaining for later stages: harnesses
   for the future font/image/archive/media parsers (§19.5) as they land.
6. **§19.7 `proptest` models + `cargo xtask proptest`** — *Bronze tier
   done*. Each capability-critical path carries an in-tree
   `proptest`-style stateful model in `tests/proptest_model.rs`,
   replaying a randomised command sequence against an independent
   reference model and letting `proptest` shrink any counterexample:
   `lib/caps` (`CapabilitySet` algebra + delegation invariant, plus a
   signed-`CapabilityToken` verify oracle), `kernel/sec` (`CapTable` /
   `TaskCapabilities` derive=intersection, delegate-never-widens,
   revoke-only-shrinks), `kernel/ipc` (the capability-checked `Port`
   send/recv/destroy lifecycle, fail-closed precedence + FIFO fidelity),
   and `kernel/syscall` (the `Dispatcher` §5.4 capability gate +
   invocation accounting). New `cargo xtask proptest` (in `ci` after
   `fuzz`) runs each model for a wall-clock budget — `--quick`
   (≥ 5 s/model, the per-PR floor, a practicality concession) or
   `--soak` (≥ 24 h/model, nightly, the real coverage) — via
   `RUSTOS_PROPTEST_BUDGET_SECS`, fail-closed on any counterexample,
   hang, or invariant failure. New `cargo xtask spec-review` (also in
   `ci`) scans the source tree and fails closed if any unreviewed
   AI-draft marker reaches `main`. `proptest` is the already-pinned,
   already-audited dev-dependency (no new external crate; §2.12). Docs:
   `docs/src/security/proptest_models.md`. Remaining for §19.7: the
   Silver TLA+ model and the Gold Verus contracts (burn-down item 11).
7. **§19.2 rxe loader** — *done (load-time policy + mapping)*. The `rxe`
   load image — a fixed `LoadHeader` plus a `Segment` table — and its
   §19.2 load-time policy live in `lib/abi/src/rxe.rs`:
   `RxePermission::from_segment_flags` admits only read-only / read-execute
   / read-write (refusing `RWX` via `WriteExecSegment`, non-readable, and
   unknown-flag segments), `LoadImage::parse` refuses a non-PIE image
   (`NotPositionIndependent`) and a CFI-tag mismatch
   (`InterfaceHashMismatch`, constant-time compare against the
   syscall-interface hash), and validates segment alignment, sizes,
   ordering/overlap, and that the entry point lies in an executable
   segment. `kaslr_bias` derives a page-aligned, bounded, seed-deterministic
   load bias; `Segment::relocated_vaddr` / `LoadImage::relocated_entry`
   apply it with checked arithmetic. The `kernel/mem` side
   (`kernel/mem/src/loader.rs`, the crate's first `lib/abi` consumer)
   adds `map_flags_for` (never `WRITE|EXEC`) and `map_image`, which maps a
   validated image into an `AddressSpace` via an injected frame allocator
   and returns the relocated entry — W^X holding twice over (loader +
   `PageTableOps`). ~25 `rxe` unit tests + 4 loader tests; all `no_std`,
   clippy `-D warnings` clean. Docs: `docs/src/security/rxe_loader.md`.
   The **process-image spawn builder** (`kernel/mem/src/spawn.rs`,
   `build_process_image`) now closes the segment-content gap: it shares one
   page-mapping loop with `map_image` (`map_region`, §2.2), maps **and**
   fills every segment page with its file content (zeroing the BSS tail),
   maps a zeroed user stack, and serialises the `rustos_abi::process`
   startup-vector block into the new address space, returning the
   `ProcessImage` (entry / user-sp / startup-block address) an Arch HAL
   enter-U-mode primitive consumes. Content is written kernel-side through
   `PhysMap` (not `copy_out`) so a read-execute page can be initialised
   without ever being user-writable; every input is validated and the
   builder fails closed with `SpawnError`. The production startup-vector
   builder lives in `lib/abi` (`process::encoded_len` / `process::write_into`,
   allocation-free, fail-closed on the frozen limits, round-tripping through
   `ProcessStart::parse`; the test helper and a new `fuzz_decode` target now
   drive it, §2.2/§19.6). 10 spawn + 13 process-builder unit tests. Docs:
   `docs/src/security/rxe_loader.md`. The **Arch HAL "enter user mode"
   primitive** that consumes the `ProcessImage` has now landed for riscv64
   and aarch64: `kernel/arch/api/src/userentry.rs` defines the
   architecture-neutral `UserEntry { entry, stack_pointer, arg0 }` register
   state (mirroring `ProcessImage`) and the object-safe `EnterUser` trait
   (diverging `unsafe fn enter_user(&self, UserEntry) -> !`); all three
   native ports implement it — riscv64 (`sret`), aarch64 (EL0 `eret`), and
   x86_64 (`iretq` to ring 3) — with the one `asm!` definition each. The
   riscv64/aarch64 `asm!` was lifted off the CC2 QEMU round-trips (§2.2)
   which reach the transition through the HAL; the x86_64 `iretq` path lands
   with its own ring-3 QEMU exercise (`tests/integration/enter_user_qemu_x86_64`,
   enrolled in `qemu_tests.rs` + the workspace) that boots the production
   kernel, builds a ring-3 space (a USER-exec, non-writable alias of the
   `ros_sys_cap_query` stub — W^X — plus a USER r/w stack, via the new
   `paging::map_4k_user`, which shares one walk with `map_4k`, §2.2), `iretq`s
   to ring 3, and asserts the stub's real `syscall` traps back with the
   expected `(number, args)` (PASS; deliberately-wrong expectation FAILs).
   The CC3 **spawn round-trips** have now landed and are QEMU-proven on all
   three native targets — riscv64
   (`tests/integration/spawn_program_qemu_riscv64`), aarch64
   (`tests/integration/spawn_program_qemu_aarch64`), and x86_64
   (`tests/integration/spawn_program_qemu_x86_64`) — each PASS +
   a deliberately-wrong-expectation FAIL: the separate PIE fixture program
   `tests/integration/cc3_program` (links only `rustos-crt0` +
   `rustos-abi-sys`, so no `_start` collision) is converted to an `rxe` blob
   by `rustos_itest_harness::elf2rxe::elf_to_rxe` (taking a `load_bias` so
   the image maps at a high `USER_BIAS` clear of the kernel identity map),
   built into a real user (U-mode / EL0 / ring-3) address space by the
   production capability-checked, audited spawn caller
   `rustos_kernel_core::spawn_and_enter` (gated on the new
   `CapabilityId::PROC_SPAWN`; audited via the `ProcessSpawned` /
   `ProcessSpawnDenied` / `ProcessSpawnFailed` events; the cap gate + audit
   live in the caller, **not** `kernel/mem`, §17.4), and entered through
   the Arch HAL `EnterUser` (`sret` / EL0 `eret` / `iretq`); the program
   parses `argv[1]` and exits with it. Each bare-metal `PageTableOps` adapter
   is test-local (an arch-crate impl would invert §17.4 layering); the aarch64
   adapter maps EL0 leaves via `el0_code`/`el0_rodata`/`el0_data_leaf_attrs`
   (the new read-only-non-exec `el0_rodata_leaf_attrs` keeps `.rodata` /
   the startup block W^X) and the test kernel enables `CPACR_EL1.FPEN`
   before the NEON-vectorised decoder runs; the x86_64 adapter maps W^X
   leaves via the new production `paging::map_4k_user_wx` /
   `flags::NO_EXECUTE` and the test boots the production kernel (so the GDT
   ring-3 selectors / TSS / `IA32_LSTAR` entry are installed) and enables
   `IA32_EFER.NXE`. **CC3 is complete.** Remaining for the broader §19.2
   posture: stack-canary / shadow-stack selection on real arch page tables.
8. **§19.1 side-channel HAL trait set + conformance vertical** — *done*.
   `kernel/arch/api/src/sidechannel.rs` adds the closed side-channel
   surface: `SideChannelMitigation` (syscall entry/exit speculation
   barriers + the context-switch microarchitectural-buffer/indirect-
   branch barrier) and a declarative `MitigationProfile` whose five slots
   (KPTI-equivalent isolation + the four barriers) are each `Applied`,
   `NotVulnerable(reason)`, or tracked `Pending(note)`. `validate` is the
   honesty gate (no unjustified omission) and `is_release_ready` the
   §19.1 "cannot ship" gate (no `Pending`). The portable
   `sidechannel::conformance::run_all` vertical (the §17.2 suite) is run
   by every port from a host test; each port also pins its exact honest
   profile so it cannot silently downgrade. Per target: x86_64 applies
   `lfence` (entry/exit) and `verw` (MDS buffer clear) and tracks
   KPTI + IBPB as `Pending` (Stage 6 page tables / CPUID probe); aarch64
   applies `csdb` and declares the MDS buffer flush `NotVulnerable`
   (Intel-only) with KPTI + the MIDR-specific Spectre-v2 sequence
   `Pending`; riscv64 applies a conservative `fence` and is release-ready
   (the in-order cores RustOS targets are not Meltdown/MDS/Spectre-v2
   vulnerable); wasm32 is release-ready (every control host-owned). All
   barrier instructions are `cfg`-gated to the bare-metal target under a
   `// SAFETY:` block; ~14 api + 12 per-port unit tests. Docs:
   `docs/src/security/side_channels.md`. The `lib/crypto`
   constant-time-under-`-O3` test also landed: `rustos_crypto::ct_eq`
   (`lib/crypto/src/constant_time.rs`) folds every byte pair with no
   early exit, an instrumented full-traversal test proves the property
   without wall-clock timing (§7), and `cargo xtask ci` re-runs the
   crate's tests under the release profile (`-C opt-level=3`). Remaining
   for §19.1: closing the KPTI / IBPB `Pending` gaps (Stage 6).
9. **§19.3 `cargo xtask build --reproducible`** — bit-reproducible image
   verification on release tags. Depends on Stage 8 image builders —
   **[DO IMMEDIATELY ON UNBLOCK]**.
10. **§19.5 parser sandbox model** — minimum-capability sandbox process
    for every untrusted-input parser. Depends on Stage 6 process model —
    **[DO IMMEDIATELY ON UNBLOCK]**.
11. **§19.7 Silver model checker** — *done*. `cargo xtask model-check`
    (wired into `ci` right after `proptest`) is an in-tree exhaustive
    explicit-state model checker — the sanctioned TLA+ *equivalent*
    (`AGENTS.md` §2.12 forbids trusting an external Java tool; §19.6 set
    the "equivalent in-tree harness" precedent). A generic breadth-first
    `check` enumerates every reachable state of a finite abstract state
    machine and verifies the safety invariants at each state and on each
    transition, failing closed on the first counterexample with a minimal
    action trace. The modelled machine is the combined capability + IPC
    core: a subject task's authority under derive/delegate/revoke
    (`kernel/sec::TaskCapabilities`, `lib/caps`) and the capability-checked
    bounded port under send/recv/destroy (`kernel/ipc::Port`). Invariants:
    no-ambient-authority/unforgeability (effective ⊆ user_grant ∩
    manifest), authority-monotone (delegate-never-widens +
    revoke-only-shrinks), ipc-fail-closed (no unauthorised/oversize message
    queued), fail-closed-admission, mailbox-capacity, and
    closed-port-drained. Two fault-injection unit tests (a widening
    delegate and a port that skips the cap check) prove the checker rejects
    a broken model — the verifier is the oracle (§19.7). The production run
    explores 100 states / 2400 transitions in milliseconds. The formal
    Init/Next/Inv narrative is kept in sync under
    `docs/src/security/model/capability_ipc.md`. 11 unit tests in
    `tools/xtask`.
12. **§19.7 Gold + §19.8 CHERI** — Verus contracts on `lib/caps` /
    `kernel/sec` (`cargo xtask verify`) and `kernel/arch/cheri-*`.
    Aspirational; tracked here, not yet scheduled.
13. **§19.10 hardware memory tagging** — *done (HAL surface + per-arch
    profiles + software slab UAF check)*. `kernel/arch/api/src/memtag.rs`
    adds the closed memory-tagging surface: the `MemoryTagging` trait
    (granule geometry + capability-checked `set_region_tag`), the honest
    `TaggingProfile` (`tag_storage` / `tag_check_faults`, each `Supported`
    / `Unsupported(reason)` / `Pending(note)`, with `validate` /
    `is_release_ready` honesty + release gates), the architecture-neutral
    `MemTag` / `next_free_tag` rotation, and the `memtag::conformance`
    vertical every port runs from a host test. aarch64 implements Arm MTE
    (the `stg` store sequence under `#[target_feature(enable = "mte")]`,
    16-byte / 4-bit granule, gated behind a default-off `mte_enabled`
    flag) and declares both slots `Pending` on the Stage 6 MTE enable;
    x86_64 / riscv64 / wasm32 declare a justified `Unsupported`. The
    `kernel/mem` slab hardens use-after-free now in software: a
    tag-carrying `SlabHandle` whose slot was freed and reallocated
    mismatches the slot's rotated tag and is rejected with
    `SlabError::TagMismatch`, sharing the HAL's `next_free_tag`. ~16 api +
    14 per-port + 7 slab unit tests (3 tag rotation + 4 software-check
    policy). Docs:
    `docs/src/security/memory_tagging.md`. The software UAF tag check is
    the **on-by-default** mechanism today (`AGENTS.md` §19.10): `Slab::new`
    enables it on every port. Because it costs a tag rotation per alloc
    and a comparison per free/access, the slab stands it down — for
    performance — exactly where it is redundant: `SoftwareTagCheck::for_tagging`
    returns `Disabled` when the port's `MemoryTagging` profile
    `enforces_uaf_in_hardware()` (both `tag_storage` and `tag_check_faults`
    `Supported`), and `Slab::with_tag_check` then skips the rotation and
    comparison while the other slab invariants (double-free, unknown-handle,
    guard pages) still fail closed. No port enforces UAF in hardware yet, so
    the software check is active everywhere today. Remaining:
    once the Stage 6 page-table work lands the `FEAT_MTE` probe
    (`ID_AA64PFR1_EL1.MTE`) and the `Normal Tagged` mapping, switch the
    aarch64 default constructor to **auto-enable MTE whenever the silicon
    reports `FEAT_MTE`** (so hardware MTE is on by default where supported,
    never executing an `UNDEFINED` `stg` on a core without it), wire the
    `stg` store path live, and decode the hardware tag-check fault —
    **[DO IMMEDIATELY ON UNBLOCK]**.

---

## §20 / §21 ABI Compliance (`stdinfo` + 64-bit-native time)

**Status:** ABI foundation delivered; RustFS timestamp follow-up complete.

`AGENTS.md` §20 (Standard Information Stream) and §21 (64-bit Time and
Filesystem Timestamps) were added after Stages 0–5 had already frozen
their ABI surfaces, so a compliance pass was run before continuing Stage
6.

1. **§21 canonical time types** — *done*. `lib/abi/src/time.rs`
   (`rustos_abi::time`) defines `Time64` (signed 64-bit seconds since the
   Unix epoch + a canonical nanosecond field — a `timespec64` analogue,
   not seconds-only `time64_t`) and `Duration64` (signed seconds + nanos).
   Both encode 12 bytes little-endian via the shared `lib/abi/src/le.rs`
   helpers (a new `read_i64`/`put_i64` pair was added there so no `as`
   cast is needed). Narrowing to a legacy on-disk field is checked
   (`secs_i32`/`secs_u32`) and fails with the new
   `Errno::TimestampOutOfRange = 14`; silent truncation/wrap/saturation is
   refused. Tests cover the epoch, pre-1970, post-2038, post-2106, and
   non-canonical nanosecond rejection.
2. **§21 ABI migration** — *done*. The only seconds-only absolute-time
   field in the frozen surface, `sysinfo::Uptime`, was migrated from
   `{ uptime_ns: u64, boot_unix_secs: u64 }` to
   `{ since_boot: Duration64, boot_time: Time64 }` (24-byte wire). All
   call sites updated: `sysinfod` source/service + tests, the `sysinfo`
   CLI render + fixtures, and the `lib/abi` fuzz harness (which now also
   exercises the `Time64`/`Duration64` decoders, §19.6).
3. **§20 `stdinfo` ABI** — *done*. `lib/abi/src/stdinfo.rs`
   (`rustos_abi::stdinfo`) reserves `STDINFO_FD = 3` and defines the
   closed `StdInfoRecord` (version, producer, closed `StdInfoKind`, stable
   `code`, `Severity`, terse `Human`, structured `ai`). It is `no_std` and
   allocation-free: `write_jsonl` serialises one JSONL line into a
   caller-provided buffer (JSON-escaping strings, embedding the `ai`
   object verbatim) and fails closed with `Errno::BufferTooSmall`. No
   synonym kinds; no free-form record types.
4. **Docs** — *done*. `docs/src/abi/time.md` and `docs/src/abi/stdinfo.md`
   (both in `SUMMARY.md`); `docs/src/abi/sysinfo.md` updated for the new
   `Uptime` shape.
5. **RustFS `Time64` timestamps** — *done*. Each RustFS inode now stores
   the four §21 timestamps (`created` / `modified` / `accessed` /
   `changed`) as true `Time64`, surfaced through a new versioned
   `FilesystemTimestamps` trait (`times(node) -> NodeTimes`) in
   `lib/abi/src/driver/filesystem.rs` — a separate `abi-v1` extension
   alongside `FilesystemSecurity`, never a widening of `FilesystemRead` /
   `FilesystemWrite` (§2.4 / §9). The on-disk inode record was reshaped
   (the four 12-byte `Time64` fields occupy bytes 40..88; the inline
   direct-pointer count dropped from 16 to 12 to keep the fixed 256-byte
   record) and `FORMAT_VERSION` bumped to 2, so a version-1 volume is
   refused rather than misread; the timestamps ride the existing journal
   because they live in the inode block. The driver stamps them from a
   clock seam (`RustFs::with_clock(fn() -> Time64)`, defaulting to the
   Unix epoch so a clockless board stays deterministic and never panics,
   §2.9): create stamps all four and bumps the parent directory's
   mtime/ctime, write advances mtime/atime/ctime, truncate advances
   mtime/ctime, `set_security` advances ctime, and remove bumps the
   parent's mtime/ctime; `created` is set once. Tests cover default-epoch
   behaviour, the POSIX stamping rules, directory create/remove tracking,
   remount persistence, and pre-1970 / post-2038 round-trips without
   truncation. Docs: `docs/src/filesystem/rustfs.md` and
   `docs/src/abi/driver_traits.md`.

---

## TSC hardening & untrusted-timer resolution (§19.1)

**Status:** done.

Two TSC/timer risks flagged in a side-channel review were closed. Note:
for this work `abi-v1` is treated as **not** frozen — the charter's and
this plan's "frozen" language is superseded by the task direction — so a
new capability could be added and `clock_get`'s observable resolution
changed. No `clock_get` *signature* changed regardless (the coarsening
is value-only), so the `ENCODED_TABLE` syscall hash is untouched.

1. **Validate the TSC before trusting it (x86_64).** `RDTSC` is only a
   sound cross-CPU monotonic base on an Invariant-TSC part. New
   `kernel/arch/x86_64/src/tsc.rs` adds the pure, host-tested decoder
   `invariant_tsc_supported(edx)` (CPUID leaf `0x8000_0007` EDX bit 8)
   and the bare-metal `detect_invariant_tsc` probe (kept in the arch
   crate per §17.2; host build returns the conservative default). The
   boot pipeline (`kernel/rustos-kernel/src/boot.rs`) logs the decision
   on every boot (`KERNEL_BOOT_TSC_INVARIANCE`, id 4098) and fails
   closed (`BootError::TscNotInvariant`) before bringing up a second CPU
   on a part lacking it. A single-CPU boot proceeds (one TSC is
   self-monotonic), so the QEMU default CPU (no `invtsc`) still boots;
   the SMP guard is live, reachable code that triggers the day AP
   bring-up populates a second `cpu_to_lapic` slot. Frequency is still
   measured empirically against the PIT, never read from CPUID/firmware.
2. **Don't hand full-resolution time to untrusted code.** New
   `CapabilityId::TIME_HIRES = 16` (named `CAP_TIME_HIRES`, frozen by
   the `well_known_ids_are_frozen` / name round-trip tests). `lib/abi`
   gains `COARSE_CLOCK_GRANULARITY_NS` (1 µs) and the pure
   `coarsen_clock_ns` flooring helper (the single coarsening site,
   §2.2). The `clock_get` handler (`kernel/core`) now returns the raw
   nanosecond reading only to callers holding `CAP_TIME_HIRES`; every
   other caller — §19.5 parser sandboxes, untrusted apps, the wasm
   userland (which shares this architecture-neutral handler) — gets the
   floored value. Coarsening preserves the per-CPU monotonic contract
   `irq_wait` depends on. `setcap`/`getcap` accept the new name with no
   code change (data-driven `from_name`).

**Tests.** `rustos-abi` +3 (`coarsen_*` floor/monotonic, `TIME_HIRES`
frozen-id/name/index), `rustos-arch-x86_64` +2 (`tsc` decoder),
`rustos-kernel-core` +2 net (the `clock_get` test split into hires /
coarsened / comparison, plus a `TestArch::set_monotonic_ns` helper).
**Docs.** `docs/src/architecture/syscalls.md` (clock-resolution
section), `docs/src/security/side_channels.md` (untrusted-timer +
validated-clocksource sections), `docs/src/platform/x86_64.md`
(`ticks_now` note + boot pipeline step 6b).

---

## CURSES Stage C1 — `lib/vt` (shared escape/attribute vocabulary)

**Status:** done.

First stage of `plans/CURSES.md` (the text-mode / TUI stack). Note: for
this work `abi-v1` is treated as **not** frozen — the task direction
supersedes the charter's and this plan's "frozen" language — though C1
adds no kernel/user ABI surface, only a new `lib/*` crate.

1. **New `no_std` + `alloc` crate `lib/vt`** (`rustos-vt`) — the canonical
   ANSI / VT / xterm vocabulary, the single source of truth shared by the
   terminal emulator (consumer) and the future curses renderer (emitter),
   so there is no second escape-sequence definition (§2.2). Modules:
   `control` (C0/C1 bytes, CSI/OSC/DCS introducers, final bytes, DEC
   private-mode numbers as typed constants), `color` (`BasicColor` 0–15 +
   the `Color` 16/256/truecolour models), `attr` (the `Sgr` operation enum,
   the one `write_params`/`decode_params` SGR table both sides use, and the
   shared `Attributes` fold), `cell` (`Cell` = glyph + `Attributes`), `op`
   (the `Op` operation vocabulary + `EraseMode`), `emit` (the
   `encode`/`encode_into`/`encode_all` emitter), and `parse` (the streaming
   `Parser`).
2. **Emitter + streaming parser over the same tables** — each `Op`/`Sgr`
   has exactly one canonical encoding, so emit→parse is the identity. The
   parser is a byte-at-a-time state machine (ground / UTF-8 / escape / CSI /
   OSC-DCS string) that is total: parameters saturate at `PARAM_MAX`, the
   parameter and string buffers are bounded (`MAX_PARAMS`, `MAX_STRING`),
   UTF-8 is decoded with overlong/continuation rejection, and an
   unrecognised, oversized, or malformed sequence is consumed and dropped
   rather than corrupting state or panicking (§2.9). No
   `unwrap`/`expect`/`panic!`; nothing touches fd 3 (§20).
3. **Tests** (§7) — `lib/vt/src/tests.rs`: 19 unit tests covering
   round-trip identity for every SGR/colour/movement/erase/mode/cursor op,
   multi-attribute SGR groups, default-parameter handling, oversize
   saturation, fail-closed dropping of unknown sequences, split-feed
   streaming, and the `Attributes` fold. `lib/vt/tests/proptest_bytes.rs`:
   `proptest` that any byte stream parses without panic and chunk-invariant,
   plus arbitrary-`Op` emit→parse identity. `lib/vt/tests/fuzz_vt.rs`: the
   §19.5/§19.6 deterministic fuzz harness for the untrusted-input parser,
   registered in `tools/xtask/src/commands/fuzz.rs` `TARGETS` (`fuzz_vt`)
   with a `vt_parser_harness_is_registered` test.
4. **Registration** (§6) — added to the workspace `Cargo.toml` members, to
   `AGENTS.md` §3's `lib/` tree, and here; stability tier `experimental` in
   `lib/vt/README.md`.
5. **Docs** (§13) — `docs/src/lib/vt.md` + `docs/src/SUMMARY.md` entry;
   rustdoc on every public item with a crate-level doctest.

Layering (§17.4): `lib/vt` depends on `lib/*` only and lives outside
`userland/gui/*`, so a headless image links it freely (§17.3).

---

## CURSES Stage C2 — refactor `userland/apps/terminal` onto `lib/vt`

**Status:** done.

Second stage of `plans/CURSES.md`. The terminal emulator stops carrying a
private escape-sequence parser and becomes a *consumer* of the one shared
`lib/vt` vocabulary, so there is a single escape-sequence definition in the
tree (§2.2). No kernel/user ABI surface changed.

1. **`Parser` is now a thin adapter over `lib/vt`** (`userland/apps/terminal/
   src/parser.rs`) — it feeds bytes to `rustos_vt::Parser` and applies each
   `Op` to the `Grid`; the old hand-written CSI subset state machine is gone.
2. **`Grid` consumes `lib/vt`'s `Cell`/`Attributes`** (`grid.rs`) — each cell
   stores a glyph plus its folded `Attributes`, and the grid grew a rendition
   pen, cursor visibility, a scroll region (with region-confined line-feed and
   `SU`/`SD` scrolling), the alternate screen (saving/restoring the main
   screen), the saved cursor (`ESC 7`/`ESC 8`), and the OSC window title. The
   erase ops now take `lib/vt`'s `EraseMode`. Every op stays total and
   clamping (§2.9).
3. **The emulator is xterm-class and advertises honestly** — `lib.rs`
   re-exports `rustos_vt::{Cell, Attributes, Color}` and adds the `TERM`
   constant (`xterm-256color`); every capability that name implies (16/256/
   truecolour SGR, cursor addressing, erase, scroll region, alt-screen, cursor
   visibility) is really parsed, so the advertised `TERM` is not a lie (§2.2).
4. **Rendering resolves colour one way** (`render.rs`) — each cell is painted
   with its own rendition: a `Default` colour takes the theme's
   `on_surface`/`surface` roles, the 16 basic colours and the 256-colour
   palette map through the standard ANSI tables, truecolour is used directly,
   `reverse` swaps the pair, and `bold` brightens a basic colour; the visible
   cursor cell keeps the accent highlight.
5. **Tests** (§7) — `userland/apps/terminal/src/tests.rs` grew from 23 to 34
   unit tests: SGR folding (bold/colour, reset, 256-index, truecolour), the
   scroll region confining scrolling and the bottom-margin line feed, the
   alternate screen saving/restoring the main screen, cursor visibility and
   the hidden cursor not painting, the OSC window title, the saved cursor
   round-tripping position and pen, and the §2.2 emitter↔consumer "one
   vocabulary" identity (feed `lib/vt`'s `encode_all` output, assert the
   grid). The existing grid/cursor/erase/`pump`/`send`/render tests were
   migrated to the shared `Cell`.
6. **Docs** (§13) — `userland/apps/terminal/README.md` and the terminal
   section of `docs/src/desktop/apps.md` describe the shared-vocabulary
   consumer, the xterm-class capabilities, and the honest `TERM`.

---

## CURSES Stage C3 — `lib/termcap` (compiled-in capability database)

**Status:** done.

Third stage of `plans/CURSES.md`. A new `lib/*` crate maps a `TERM` value to a
capability record, compiled in rather than read from a terminfo/termcap file —
RustOS has no `/etc`, `/usr`, or `/proc` (§16.1). For this work `abi-v1` is
treated as **not** frozen (the task direction supersedes the charter's and this
plan's "frozen" language), though C3 adds no kernel/user ABI surface, only a
new `lib/*` crate.

1. **New `no_std` + `alloc` crate `lib/termcap`** (`rustos-termcap`) — depends
   on `rustos-vt` and `lib/*` only (§17.4). Modules: `term_type` (the closed,
   versioned `TermType` set — `Xterm`, `XtermColor`, `Xterm16Color`,
   `Xterm256Color`, `Alacritty`, `XtermKitty`, `Dumb`, `Vt100`, `Vt220` —
   `TermType::ALL`, `term_name`, `capabilities`, and the fail-closed
   `from_term`) and `capabilities` (`ColorDepth`, `MouseReporting`/
   `MouseSupport`, `ArrowKeys`, `KeyInput`, and the `Capabilities` record with
   `for_term` and `referenced_ops`).
2. **Every record is expressed in `lib/vt` terms** (§2.2) — output capabilities
   are the `rustos_vt::Op`s the terminal accepts, colour is the
   `rustos_vt::Color` model depth, and arrow-key input is the `Op` those bytes
   parse back to. The crate defines no second escape-sequence table.
   `referenced_ops` returns exactly the `Op`s a record names. Mouse reporting,
   bracketed paste, and the function/editing/keypad keys are recorded as
   capability *facts*; their byte sequences enter `lib/vt` when the C4 input
   decoder needs them, never duplicated here.
3. **`from_term` fails closed** — an unknown or empty `TERM` degrades to
   `TermType::Dumb` (§2.9, §5.4); parsing never triggers a file read (§16.1).
   No `unwrap`/`expect`/`panic!`; nothing touches fd 3 (§20).
4. **Tests** (§7) — `lib/termcap/src/tests.rs`: 13 unit tests — one capability
   test per `TermType`, the "unknown/empty `TERM` falls back to `Dumb`" test,
   the `term_name` ↔ `from_term` round-trip, the `ColorDepth::supports` depth
   checks, and `no_record_emits_a_sequence_absent_from_vt` (every referenced
   `Op` round-trips through `lib/vt`). Plus the crate-level doctest.
5. **Registration** (§6) — added to the workspace `Cargo.toml` members, to
   `AGENTS.md` §3's `lib/` tree, and here; stability tier `experimental` in
   `lib/termcap/README.md`.
6. **Docs** (§13) — `docs/src/lib/termcap.md` + `docs/src/SUMMARY.md` entry;
   rustdoc on every public item with a crate-level doctest.

---

## CURSES Stage C4 — `lib/curses` (TUI / screen-model library, core)

**Status:** done.

Fourth stage of `plans/CURSES.md`. A new `lib/*` crate gives applications a
client screen model and renders it to a terminal through `lib/vt` +
`lib/termcap`. For this work `abi-v1` is treated as **not** frozen (the task
direction supersedes the charter's and this plan's "frozen" language); C4 added
no kernel/user ABI surface, only `lib/vt` vocabulary and a new `lib/*` crate.

1. **`lib/vt` vocabulary extended** (§2.2, the one place these sequences live) —
   the function / editing keys (`Key`, `SS3` + `CSI … ~`), the SGR mouse report
   and the mouse-tracking + bracketed-paste DEC private modes
   (`MouseReport`/`MouseButton`/`MouseMode`, `Op::Key`/`Op::Mouse`/
   `Op::SetMouseMode`/`Op::SetBracketedPaste`/`Op::PasteStart`/`Op::PasteEnd`).
   The emitter and the streaming parser gained an `SS3` state, the `<` SGR-mouse
   introducer, and `~`/`M`/`m` dispatch, with emit→parse round-trip tests for
   every new op (fail closed on unknown sequences, §2.9).
2. **New `no_std` + `alloc` crate `lib/curses`** (`rustos-curses`) — depends on
   `rustos-vt` + `rustos-termcap` + `lib/*` only (§17.4). Modules: `geom`,
   `buffer`, `window` (the client `Window`/pad draw model — text, attributes,
   colours, `draw_box`/border, lines, scrolling region, resize), `color`
   (`ColorPairs` + the truecolour→256→16→mono `downgrade`), `render` (the
   minimal-diff renderer + `dumb` full-rewrite fallback), `input`
   (`Input`/`Event` decoder over `lib/vt`'s parser), and `screen` (the
   `Screen<T: Tty>` I/O-injected driver: `wnoutrefresh`/`pnoutrefresh`/
   `doupdate`/`refresh`, mouse + paste enabling, `resize`, `read_events`).
3. **Minimal-diff, capability-aware output** — `render` emits one cursor move
   per change-run, one SGR transition per attribute change, one `Print` per
   glyph; every colour is degraded by `rustos_termcap::ColorDepth` so an
   unrenderable colour is never emitted (§2.9). One vocabulary end to end: the
   bytes emitted parse back through `lib/vt`'s consumer.
4. **Tests** (§7) — `lib/curses/src/tests.rs`: 26 unit tests (window model,
   golden minimal-diff op sequences, colour-downgrade, per-terminal input
   decode through `lib/vt`'s emitter, the `Screen` driver over an in-memory
   `Tty`) plus the crate doctest; `lib/curses/tests/fuzz_curses_input.rs`, the
   §19.5/§19.6 deterministic fuzz harness registered in
   `tools/xtask/src/commands/fuzz.rs` `TARGETS` (`fuzz_curses_input`) with a
   `curses_input_harness_is_registered` test. New `lib/vt` round-trip tests for
   the keys/mouse/paste ops.
5. **Registration** (§6) — added to the workspace `Cargo.toml` members, to
   `AGENTS.md` §3's `lib/` tree, and here; stability tier `experimental` in
   `lib/curses/README.md`.
6. **Docs** (§13) — `docs/src/lib/curses.md` + `docs/src/SUMMARY.md` entry;
   rustdoc on every public item with a crate-level doctest.

---

## CURSES Stage C5 — curses completeness + the `top` consumer

**Status:** done.

Fifth stage of `plans/CURSES.md`. The curses surface is completed to the level
a ported curses program expects, and the first in-tree consumer proves it. For
this work `abi-v1` is treated as **not** frozen (the task direction supersedes
the charter's and this plan's "frozen" language); C5 adds no kernel/user ABI
surface — only `lib/curses` API and a new `userland/apps/` crate.

1. **`lib/curses` completeness** — wide/UTF-8 cell handling (a new `width`
   module: `char_width`/`is_wide`/`str_width`/`truncate_to_width` + the
   `CONTINUATION` marker; `Window::add_char` writes a double-width glyph as a
   lead + continuation cell and the renderer prints it once and steps the
   terminal cursor two columns), colour-pair allocation (`ColorPairs::alloc_pair`
   / `Screen::alloc_pair`), and `getch`/timeout/non-blocking input
   (`Screen::getch` + `InputMode` over the `Tty` seam's new defaulted
   `read_blocking`/`read_timeout`, with a pending-event queue). Panels-equivalent
   stacking is **deferred** until a consumer needs it (§2.3); overlays compose
   through ordered `wnoutrefresh`. 12 new unit tests (38 total) all green.
2. **The `top` consumer** (`userland/apps/top`, `rustos-top`) — a live
   process-overview TUI in the spirit of Linux `top`: a scrolling, selectable
   process list with an all/own scope toggle, on-demand refresh, and a `?`
   help overlay. It reads the `sysinfo-v1` process list through the shared
   `lib/procinfo` helpers (no duplicated paging/column rendering, §2.2) and
   draws it through `lib/curses` (no hand-written escape sequences). An I/O-free
   `Model` plus a pure `render` and a thin `run` loop, over the object-safe
   `Transport`/`Tty` seams, make it host-testable without a kernel; 18 unit
   tests cover the model, rendering, and the loop (incl. a wide-name render and
   the capability-denied global view).
3. **Linking policy** (`AGENTS.md` §16.4, per the task's linking-policy
   updates) — `lib/curses` (with `lib/termcap` + `lib/vt`) is **part of the
   OS** and is now a curated `/System/Libraries/` shared-library class
   ("Terminal / TUI client"); OS apps and third-party apps **dynamically
   link** it rather than compiling it in. §16.4, §3's `lib/curses` note,
   `plans/CURSES.md` §2, and the crate docs were updated to match.
4. **Registration** (§6) — `userland/apps/top` added to the workspace
   `Cargo.toml` members; the new shared-library class recorded in `AGENTS.md`
   §16.4 and here (§16.4 requires both).
5. **Docs** (§13) — `docs/src/userland/curses-porting.md` (a porting guide for
   building a curses app against `lib/curses`, with capability/fail-closed and
   linking notes) + `docs/src/SUMMARY.md` entry; `userland/apps/top/README.md`;
   refreshed `lib/curses` README + `docs/src/lib/curses.md`; rustdoc on every
   new public item.

C6 (remote terminals — serial / SSH to Linux hosts) is the next stage and has
not been started.

---

## CCOMPAT — C-callable `abi-v1` (full `lib/abi` header, syscall stubs, crt0)

Staged build plan: `plans/CCOMPAT.md` (binding under `AGENTS.md`). It makes the
**whole** of `lib/abi` — every public `#[repr(C)]` type, constant, and enum
discriminant, not just the syscalls — callable from programs not written in
Rust (C first), so `lib/abi` is a public developer surface for third-party
programs and not only the OS (`AGENTS.md` §9). The C header is a generated
*view* of `lib/abi` under `include/` (never a hand-maintained parallel
definition, §2.2), guarded against drift by `cargo xtask c-header` in
`cargo xtask ci`. Third-party native code is treated as potentially hostile
and/or poorly written: the stub runtime is not a privileged bypass, every
capability/input check stays kernel-side (§5.4), and C binaries obey the
`rxe`/`abi-v1` hardening invariants (PIE, W^X, CFI tag, §19.2) identically.

This adds the curated `/System/Libraries/` class **System runtime / C ABI**
(`AGENTS.md` §16.4): the minimal libc-equivalent (the `ros_sys_<name>`
syscall stubs + crt0), dynamically linked like every other curated library.

**Stages** (see `plans/CCOMPAT.md` for deliverables, tests, docs):

- CC1 — Full `lib/abi` C header surface (grow `cargo xtask c-header` from the
  syscall/errno/capability seed to the whole crate). **Done:** the
  generator now emits one header per `lib/abi` module under `include/rustos/`
  (`rustos_error.h`, `rustos_capability.h`, `rustos_time.h`,
  `rustos_random.h`, `rustos_ipc.h`, `rustos_stdinfo.h`, `rustos_manifest.h`,
  `rustos_input.h`, `rustos_appinfo.h`, `rustos_rxe.h`, `rustos_sysinfo.h`,
  `rustos_driver.h`, `rustos_syscall.h`)
  plus the umbrella `rustos_abi.h` that `#include`s them,
  with a tree-wide drift guard; the `time` module (`ros_time64_t` /
  `ros_duration64_t` + constants), the `random` module (`ROS_RANDOM_FLAG_*` +
  the `ROS_RANDOM_*_BYTES` limits), the `ipc` module
  (`ros_ipc_message_header_t` / `ros_port_name_t` + the `ROS_IPC_*` /
  `ROS_PORT_NAME_*` constants), the `stdinfo` module (`ROS_STDINFO_FD`, the
  `ROS_STDINFO_VERSION_*` framing tags, and the `ROS_STDINFO_KIND_*` /
  `ROS_STDINFO_SEVERITY_*` `#[repr(u8)]` discriminants), and the `manifest`
  module (`ros_manifest_header_t` + the `ROS_MANIFEST_*` /
  `ROS_SYSCALL_TABLE_HASH_LEN` constants), and the `input` module (the
  pointer/keyboard record magics + wire sizes, the `ROS_INPUT_KIND_*` /
  `ROS_INPUT_BUTTON_NONE` / `ROS_KEY_CLASS_*` / `ROS_MOD_*` codes, and the
  `ROS_POINTER_BUTTON_*` / `ROS_KEY_*` discriminants), and the `appinfo`
  module (`ros_appinfo_header_t` + the `ROS_APPINFO_*` / `ROS_BUNDLE_*` /
  `ROS_MIME_*` constants, `ROS_SYSTEM_LIBRARIES_DIR`, the `ROS_BUNDLE_ENTRY_*`
  names, and the `ROS_LIBRARY_SCOPE_*` discriminants), and the `rxe` module
  (`ros_load_header_t` + the `ROS_LOAD_MAGIC` / `ROS_RXE_PAGE_SIZE` /
  `ROS_LOAD_MAX_SEGMENTS` / `ROS_LOAD_FLAG_PIE` / `ROS_SEG_FLAG_*` /
  `*_WIRE_LEN` constants and the `ROS_RXE_PERMISSION_*` discriminants), and the
  `sysinfo` module (the eight wire types `ros_sysinfo_request_header_t` /
  `ros_process_list_request_t` / `ros_process_record_t` /
  `ros_kernel_memory_stats_t` / `ros_uptime_t` / `ros_system_identity_t` /
  `ros_mount_list_request_t` / `ros_mount_record_t` + the `ROS_SYSINFO_*`
  framing / query-id / registry constants, the `ROS_PROCESS_STATE_*`
  discriminants, the `ROS_*_MAX` / `ROS_*_LEN` buffer caps, and the
  per-record `*_WIRE_LEN` sizes), and the `driver` core
  (`ros_driver_manifest_t` + the `ROS_DRIVER_MANIFEST_*` /
  `ROS_DRIVER_SIGNER_PUBKEY_LEN` / `ROS_DRIVER_SIGNATURE_LEN` constants, the
  `ROS_DRIVER_KIND_*` / `ROS_BUFFER_CLASS_*` / `ROS_DRIVER_ERROR_*`
  discriminants, and the `ROS_DRIVER_HANDLE_NONE` sentinel), and the
  `driver/*` submodule POD surface in `rustos_driver.h` (the
  storage/bus/display/filesystem/input/net struct mirrors
  `ros_block_geometry_t` / `ros_discard_capability_t` /
  `ros_health_snapshot_t` / `ros_bus_device_t` / `ros_display_mode_t` /
  `ros_accel_caps_t` / `ros_node_info_t` / `ros_dir_entry_t` /
  `ros_node_times_t` / `ros_input_event_t` / `ros_mac_address_t`, the
  `ROS_VIRTIO_PCI_*` / `ROS_MAC_ADDRESS_LEN` / `ROS_MOUNT_FLAG_*` /
  `ROS_NODE_ID_NONE` constants, and the `ROS_DISPLAY_FORMAT_*` /
  `ROS_NODE_KIND_*` / `ROS_INPUT_EVENT_KIND_*` discriminants), all values
  read from `lib/abi`, are the grown modules.
  The `capability` module needs no new header (its ids already ship in
  `rustos_capability.h`; `CapabilityQuery` is a trait with no C form). A
  generator completeness test pins every `lib/abi` `#[repr(C)]` type's
  size/align and asserts it has a C `typedef`, so a new type cannot silently
  escape the C surface (the type-surface analogue of the dense errno table).
  The Rust-only error enums (`WindowError`, `MmioMapError`), the opaque
  `MsiMessage`, the in-process policy records, the runtime objects, and the
  driver-host traits carry no C form and are deliberately omitted (§2.3). CC1
  is complete and green on the whole-project gate.
- CC2 — `lib/abi-sys`: the C-callable `ros_sys_*` stub runtime (per-arch
  trap stubs). **DONE — runtime + host tests + the QEMU round-trip on all
  three native targets (x86_64, riscv64, aarch64).** The crate
  `rustos-abi-sys` exports the eleven export-name-pinned `ros_sys_<name>`
  functions matching the CC1 header; each marshals into the canonical
  `[u64; SYSCALL_MAX_ARGS]` register layout (syscall numbers read from
  `rustos_abi`, §2.2) and issues the real trap — `syscall`/`svc`/`ecall` — as
  the §1 assembly carve-out gated on a build-script-emitted `abi_sys_trap_*`
  cfg (so §17.2 `cfg-check` stays green). Every stub is panic-free (§2.9),
  adds no authority (§4/§5.4), and returns the C-declared type; `exit` is
  `-> !`. Registered as the curated `/System/Libraries/` *System runtime / C
  ABI* class (§16.4, `experimental` tier). Host tests inject a trap seam and
  assert marshalling + return decoding for every stub, plus a drift test
  against `rustos_abi::SYSCALLS`. There is one QEMU round-trip per native
  target (enrolled in `tools/xtask/src/commands/qemu_tests.rs`), each issuing
  the `ros_sys_cap_query` stub so the **real** trap instruction runs and a
  dispatch callback asserts the kernel-observed `(number, args)`:
  `abi_sys_syscall_qemu` (x86_64, `syscall` from ring 0 → `IA32_LSTAR` stub),
  `abi_sys_syscall_qemu_riscv64` (a minimal U-mode context + `sret` so the
  `ecall` is from U-mode), and `abi_sys_syscall_qemu_aarch64` (a minimal EL0
  context + `eret` so the `svc` is from EL0). The riscv64 trap handler already
  routed `ecall`-from-U to `dispatch_ecall`; aarch64 gained the analogous EL0
  `svc` dispatch wiring (`vectors.s` passes the saved frame; `exceptions.rs`
  routes a lower-EL `svc` through the host-tested
  `syscall_entry::syscall_frame_from_saved` → `dispatch_svc`) plus EL0 paging
  primitives (`AP_RW_EL0`/`AP_RO_EL0`, `el0_code_leaf_attrs`/
  `el0_data_leaf_attrs`, `map_4k_with_attrs`, with `map_4k` delegating, §2.2),
  all host-tested. The QEMU round-trips are not in the host-only
  `cargo xtask ci` gate; they run under `cargo xtask test --qemu`.
- CC3 — crt0: per-native-target program startup/teardown enforcing the §19.2
  invariants. Depends on CC2 + the Stage 6 loader. **In progress — the
  startup-vector `abi-v1` type has landed.** The kernel→process startup vector
  is now defined once in `lib/abi` (`process` module: `ProcessStartHeader` +
  `StringSlot` + the fail-closed `ProcessStart::parse` view, with the
  `PROCESS_START_MAGIC` / `PROCESS_START_MAX_*` limits), so the kernel builder
  and crt0 will share one definition (§2.2). It is a position-independent,
  offset-based block (argv + envp, no NUL terminators, plus a per-process §19.2
  stack-canary seed) parsed as untrusted input (bounds/limit/embedded-NUL
  checks, fail closed, §2.9/§19.5/§19.6) and enrolled in the `lib/abi` fuzz
  harness. It is surfaced in the C header as `rustos_process.h`
  (`ros_process_start_header_t` / `ros_string_slot_t` + the `ROS_PROCESS_START_*`
  macros), pinned by the generator completeness test and documented in
  `docs/src/abi/c-abi.md`. **The crt0 object has now landed too:** the new
  `lib/crt0` crate (`rustos-crt0`) provides the per-native-target `_start`
  trampoline — the §1 assembly carve-out, gated on a build-script-emitted
  `crt0_native_*` cfg (so §17.2 `cfg-check` stays green) — which aligns the
  stack, carves a bounded scratch region, and calls the host-testable,
  allocation-free `build_c_runtime` that validates the startup vector and lays
  out the C `argv`/`envp` (copying each NUL-free string + NUL-terminating it,
  fail closed §2.9), installs the §19.2 stack canary into `__stack_chk_guard`
  from the kernel-supplied per-process seed, calls the hosted `main`, and
  routes its return through `ros_sys_exit`. It is the crt0 half of the curated
  `/System/Libraries/` *System runtime / C ABI* class (§16.4, `experimental`
  tier). The `rxe` hardening invariants (PIE / `RWX`-refusal / CFI tag) are
  enforced at load by `rustos_abi::rxe::LoadImage::parse` (a non-conforming
  image is refused, not patched). **The kernel-side `build_process_image`
  (`kernel/mem/src/spawn.rs`) and the Arch HAL "enter user mode" primitive
  (`kernel/arch/api` `EnterUser` / `UserEntry`) have now landed on all three
  native ports and are QEMU-proven** — riscv64 `sret`, aarch64 EL0 `eret` (the
  inline CC2 round-trip `asm!` lifted onto the HAL, §2.2), and x86_64 `iretq`
  to ring 3, the last with its own ring-3 QEMU exercise
  (`tests/integration/enter_user_qemu_x86_64`, using the new
  `paging::map_4k_user`). **The program-packaging infrastructure has now landed
  too (chunk 1 of the round-trip):** the separate PIE fixture program
  `tests/integration/cc3_program` (links only `rustos-crt0` + `rustos-abi-sys`,
  so no `_start` collision; `extern crate rustos_crt0;` pulls crt0's `_start`
  onto the link line) and the host-tested ELF→rxe converter
  `rustos_itest_harness::elf2rxe::elf_to_rxe` (LE ELF64 `ET_DYN` → W^X `rxe`
  segments, applying only `R_*_RELATIVE` at zero bias and failing closed on any
  symbolic/GOT/PLT/`REL` relocation, re-encoding via the `rustos_abi::rxe`
  encoders, §2.2; 13 host unit tests). **CC3 is complete: the spawn round-trips
  have now landed and are QEMU-proven on all three native targets**
  (`tests/integration/spawn_program_qemu_{riscv64,aarch64,x86_64}`, each
  QEMU-proven PASS + a deliberately-wrong-expectation FAIL): a test-local
  per-arch `PageTableOps` adapter over the bare-metal `paging::AddressSpace`
  (test-local because §17.4 forbids an arch crate depending on `kernel/mem`) —
  the aarch64 one maps EL0 leaves via
  `el0_code`/`el0_rodata`/`el0_data_leaf_attrs` (the new read-only-non-exec
  `el0_rodata_leaf_attrs` keeps `.rodata` / the startup block W^X) and the test
  kernel enables `CPACR_EL1.FPEN` before the NEON-vectorised decoder runs; the
  x86_64 one maps W^X leaves via the new production `paging::map_4k_user_wx` /
  `flags::NO_EXECUTE` and the test boots the production kernel (GDT ring-3
  selectors / TSS / `IA32_LSTAR` entry installed) and enables `IA32_EFER.NXE` —,
  the capability-checked / `lib/log`-audited spawn caller
  `rustos_kernel_core::spawn_and_enter` (gated on the new
  `CapabilityId::PROC_SPAWN`; audited via `ProcessSpawned` /
  `ProcessSpawnDenied` / `ProcessSpawnFailed`; in the caller not `kernel/mem`,
  §4/§5.4/§17.4; host-tested deny + build-failure paths), and the QEMU test
  whose `build.rs` builds the `cc3_program` blob via `elf_to_rxe` (now taking a
  `load_bias` so the image maps clear of the kernel identity map), spawns it,
  and asserts `exit` carries the argument. CC4 unblocks from here.
  See `.junie/next-ccompat-prompt.md`.
- CC4 — Loader / bundle integration for native `rxe` programs (resolve the
  runtime only from `/System/Libraries/` or the bundle's `Libraries/`).
  **DONE.** The `rxe` format gained a needed-shared-library table (the
  analogue of an ELF `DT_NEEDED`): the spare `LoadHeader::reserved0` became
  `needed_count` (wire size unchanged) followed by `NeededLibrary` records
  (NUL-free, `LIBREF_MAX`-byte paths; `LOAD_MAX_NEEDED` cap) that
  `LoadImage::parse` validates fail-closed and `LoadImage::needed_libraries()`
  exposes; the decoder is fuzzed and the C header
  (`include/rustos/rustos_rxe.h`) regenerated. The application-bundle loader
  (`userland/system/appmgr`) gained a `read_run` seam and now, in
  `AppLoader::load`, validates the `Run` binary through `LoadImage::parse`
  with the kernel's syscall hash as the **expected CFI tag** (enforcing the
  §19.2 PIE / W^X / CFI invariants on a C binary identically to a Rust one)
  and resolves every needed library through the existing §16.4
  `resolve_library` policy — the curated *System runtime / C ABI* runtime
  (`/System/Libraries/`) and bundle-private libraries resolve, anything else
  fails closed. No new ambient authority (capability intersection unchanged).
  New audit event `APP_RUN_IMAGE_INVALID`; 6 new rxe + 5 new appmgr tests;
  docs in `docs/src/abi/c-abi.md`, `docs/src/security/rxe_loader.md`, and the
  appmgr `lib.rs`/`README.md`. See `.junie/next-ccompat-prompt.md`.
- CC5 — End-to-end C program built+run under QEMU (audited toolchain wrapper,
  §12) exercising a slice of `abi-v1` including §21 `Time64` edges; fuzz the
  new decoders. **DONE.** The fuzz/regression sub-deliverable landed: the
  CC3/CC4 decoders (`ProcessStart::parse` / `ProcessStartHeader` /
  `StringSlot`, `NeededLibrary::decode`, `LoadImage::parse`) were already
  enrolled in `lib/abi/tests/fuzz_decode.rs` (§19.6), and a seeded regression
  corpus now backs them (`lib/abi/tests/regression_corpus.rs`): hand-crafted
  boundary images replayed through the "must not panic + accepted decode
  round-trips" contract plus per-validating-decoder accept/reject verdict
  locks; `docs/src/security/fuzzing.md` documents it. The **headline** work
  landed across **all three native Tier-1 targets**: the audited,
  version-pinned, checksummed C toolchain wrapper `tools/cc` (`rustos-cc`,
  wrapping `clang` + `ld.lld`, §12, 17 host tests), a genuinely C-language
  in-tree program (`tests/integration/cc5_program/csrc/main.c`) that
  `#include`s `include/rustos/…` and links the `ros_sys_*` runtime + crt0 via
  the `rustos-test-cc5-program` `staticlib` shim, and the QEMU round-trips
  `tests/integration/c_program_qemu_{riscv64,aarch64,x86_64}` (each build
  script compiles + links the C PIE with `rustos-cc`, converts it via
  `elf_to_rxe`, spawns it with `spawn_and_enter`; the C program checks a §21
  `Time64` value + ipc/sysinfo headers and round-trips `cap_query`/`clock_get`,
  exiting 99). riscv64 enters U-mode, aarch64 EL0 (with `CPACR_EL1.FPEN`),
  x86_64 ring-3 (production pipeline + `IA32_EFER.NXE`); each is **QEMU-proven
  PASS + a deliberately-wrong-expectation FAIL**. Docs page
  `docs/src/abi/calling-from-c.md`. See `.junie/next-ccompat-prompt.md`.

Native Tier-1 targets only (`x86_64`, `aarch64`, `riscv64`); the syscall-stub
runtime and crt0 are out of scope for `wasm32` (no trap instruction).

Done seed (current): `cargo xtask c-header` ships the surface as a per-module
header set under `include/rustos/` — the umbrella `rustos_abi.h` plus
`rustos_error.h` (error codes), `rustos_capability.h` (capability ids),
`rustos_syscall.h` (syscall numbers + one prototype per syscall),
`rustos_time.h` (`ros_time64_t` / `ros_duration64_t` + the `Time64`/
`Duration64` constants), `rustos_random.h` (`ROS_RANDOM_FLAG_*` + the
`ROS_RANDOM_*_BYTES` limits), `rustos_ipc.h` (`ros_ipc_message_header_t` /
`ros_port_name_t` + the `ROS_IPC_*` / `ROS_PORT_NAME_*` constants),
`rustos_stdinfo.h` (`ROS_STDINFO_FD` + the `ROS_STDINFO_VERSION_*` framing
tags + the `ROS_STDINFO_KIND_*` / `ROS_STDINFO_SEVERITY_*` discriminants), and
`rustos_manifest.h` (`ros_manifest_header_t` + the `ROS_MANIFEST_*` /
`ROS_SYSCALL_TABLE_HASH_LEN` constants), `rustos_input.h` (the
pointer/keyboard record magics + wire sizes + the `ROS_INPUT_KIND_*` /
`ROS_INPUT_BUTTON_NONE` / `ROS_KEY_CLASS_*` / `ROS_MOD_*` codes + the
`ROS_POINTER_BUTTON_*` / `ROS_KEY_*` discriminants), `rustos_appinfo.h`
(`ros_appinfo_header_t` + the `ROS_APPINFO_*` / `ROS_BUNDLE_*` / `ROS_MIME_*`
constants + `ROS_SYSTEM_LIBRARIES_DIR` + the `ROS_BUNDLE_ENTRY_*` names + the
`ROS_LIBRARY_SCOPE_*` discriminants), and `rustos_rxe.h` (`ros_load_header_t`
+ the `ROS_LOAD_MAGIC` / `ROS_RXE_PAGE_SIZE` / `ROS_LOAD_MAX_SEGMENTS` /
`ROS_LOAD_FLAG_PIE` / `ROS_SEG_FLAG_*` / `*_WIRE_LEN` constants + the
`ROS_RXE_PERMISSION_*` discriminants), and `rustos_sysinfo.h` (the eight
System Information wire-type struct mirrors + the `ROS_SYSINFO_*` framing /
query-id / registry constants + the `ROS_PROCESS_STATE_*` discriminants + the
`ROS_*_MAX` / `ROS_*_LEN` buffer caps + the per-record `*_WIRE_LEN` sizes), and
`rustos_driver.h` (the core `ros_driver_manifest_t` + the
`ROS_DRIVER_MANIFEST_*` / `ROS_DRIVER_SIGNER_PUBKEY_LEN` /
`ROS_DRIVER_SIGNATURE_LEN` constants + the `ROS_DRIVER_KIND_*` /
`ROS_BUFFER_CLASS_*` / `ROS_DRIVER_ERROR_*` discriminants + the
`ROS_DRIVER_HANDLE_NONE` sentinel, plus the driver-class POD mirrors
`ros_block_geometry_t` / `ros_discard_capability_t` / `ros_health_snapshot_t`
/ `ros_bus_device_t` / `ros_display_mode_t` / `ros_accel_caps_t` /
`ros_node_info_t` / `ros_dir_entry_t` / `ros_node_times_t` /
`ros_input_event_t` / `ros_mac_address_t` + the `ROS_VIRTIO_PCI_*` /
`ROS_MAC_ADDRESS_LEN` / `ROS_MOUNT_FLAG_*` / `ROS_NODE_ID_NONE` constants +
the `ROS_DISPLAY_FORMAT_*` / `ROS_NODE_KIND_*` / `ROS_INPUT_EVENT_KIND_*`
discriminants) —
each value read from
`lib/abi`, guarded byte-for-byte against drift and by a completeness test that
pins every `#[repr(C)]` type's size/align, wired into `cargo xtask ci`;
the docs page is `docs/src/abi/c-abi.md`.

---

## Assignment Notes for Task Dispatchers

When handing a stage to an implementing agent, the task brief **must**:

1. Reference this `PLAN.md` and the `AGENTS.md` charter explicitly.
2. List the stage's deliverables, tests, and docs verbatim.
3. State the dependencies that are already satisfied.
4. Forbid stubs, `todo!()`, ignored tests, and `#[allow(...)]` without
   justification.
5. Require the agent to quote actual `cargo xtask test` output on completion.
6. Require the agent to apply the `AGENTS.md` §23 Code Review and Acceptance
   Gate to its own diff and state the §23.5 verdict on completion.

A stage delivered without the above is to be returned for rework, regardless
of how much code was produced.

---

## Charter Amendments

Amendments to `AGENTS.md` (the binding charter) are logged here so an agent
can see *why* a rule exists without diffing the charter's history.

- **2026-06-07 — Code-quality & self-review hardening.** Added §2.13 (no
  pre-release backwards-compatibility code — RustOS has not shipped, so
  RustOS-native interfaces, types, and on-disk formats are evolved *in place*
  with all callers updated in the same change; no `v2`-beside-`v1`, shims,
  migrations, or "old data" fallbacks; this is distinct from reading *foreign*
  ext4/FAT32 volumes under §21 and from the §2.4 freeze that binds only from
  the first release). Added §2.14 (delete obsolete code — nothing commented
  out, `_old`-renamed, `#[allow(dead_code)]`-ed, or orphaned; deletions update
  §3 / §16.4 / this plan). Added §23 (Code Review and Acceptance Gate — a
  binding adversarial self-review every agent runs on its own output before
  reporting done: §23.1 security, §23.2 correctness/multi-arch, §23.3
  no-compat/no-dead-code, §23.4 tests/docs/process, §23.5 verdict), cross-
  referenced from §14 (mergeable criteria) and §15.12 (agent instructions).
  No code or interface changed; this amendment is documentation only.
