# PLAN.md — RustOS Build Plan

This plan turns the requirements in `AGENTS.md` into ordered, assignable
work. Each **Stage** is delivered by a separate task (and likely a separate
agent). A stage is complete only when:

- All listed deliverables exist.
- All listed tests pass under `cargo xtask test`.
- All listed documentation is written and links cleanly.
- `AGENTS.md` rules have been observed (no hacks, no duplication, no
  weakened tests, no missing docs).

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
  `docs-check`, `abi-check`, `coverage`, `ci`, `image`.
- `docs/` mdBook scaffold.
- CI definition (`.github/workflows/ci.yml` or equivalent) running
  `cargo xtask ci` on every push.
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
  `AGENTS.md` §7 / §14: `build`, `test`, `clippy`, `fmt`, `docs-check`,
  `abi-check`, `coverage`, `ci`, `image`. `abi-check` deliberately fails
  loudly if only one half of the `lib/abi/src/syscalls.rs` ↔
  `kernel/syscall/src/table.rs` pair appears.
- `docs/` ships a mdBook scaffold (`book.toml`, `src/SUMMARY.md`,
  `introduction.md`, `contributing.md`, `architecture/overview.md`) and the
  Stage 1 per-crate `lib/*` pages.
- CI definition `.github/workflows/ci.yml` runs `cargo xtask ci` on every
  push and pull request, with cargo + xtask-helper-tool caches.
- `LICENSE-APACHE`, `LICENSE-MIT`, `README.md`, `AGENTS.md`, `PLAN.md` are
  all present at the repository root.

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
- `lib/abi` ships frozen `abi-v1` types (`Errno`, `CapabilityId`,
  `SyscallNumber`, `IpcMessageHeader`, `ManifestHeader`) plus a deterministic
  100 000-input fuzz harness in `lib/abi/tests/fuzz_decode.rs`.
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
- `kernel/sync`: spinlocks, RW locks, MCS locks, RCU-equivalent, all
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
- [x] 2.1 — `kernel/sync`: spinlocks, IRQ-safe spinlock, writer-preference
      RwLock, MCS queue lock, SeqLock, epoch-based reclamation, `Once`/
      `OnceCell`. Loom-gated concurrency tests in `kernel/sync/tests/loom.rs`,
      proptest fairness test in `kernel/sync/tests/rwlock_fairness.rs`,
      decision tree in `docs/src/architecture/sync.md`.
- [x] 2.2 — `kernel/mem`: buddy/bitmap `FrameAllocator` honouring a typed
      `BootMemoryMap`, per-process `AddressSpace<P: PageTableOps>` with a
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
          `kernel/sync` (`rwlock.rs` module docs: "Process /
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
- 3d — `kernel/arch/wasm32` (browser sandbox; cooperative scheduling backed
  by `requestAnimationFrame` / `MessageChannel`; "MMU" enforced by WASM
  memory isolation between worker contexts).

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
- Stage 3b/3c/3d (aarch64 / riscv64 / wasm32) remain outstanding
  per their own checklists; each follows the same per-arch
  template (a)..(d) the x86_64 sub-stage just completed.

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

      *Carry-over:* the `kernel/sync::RwLock` process-context rule
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
          follow-up**: the kernel has no named-port registry yet
          (see `kernel/ipc/src/lib.rs` rustdoc). The handler
          returns `Errno::NotFound` and emits an audit record
          flagging "named-port registry not landed". The follow-up
          to the follow-up (Stage 5 prerequisite) lands the
          registry.
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

- [ ] **(f6)** **QEMU integration test** — extend
      `tests/integration/kernel_arch_boot/` (or add a sibling)
      that spawns a single kernel task whose first instruction is
      `syscall NR_CAP_QUERY, CAP_TIME_SET`, then `syscall NR_EXIT, 0`.
      The audit-observer sink flips `qemu_exit::exit_success`
      after observing one `AuditEvent::SyscallInvoked` for
      `cap_query` *and* one `AuditEvent::SyscallInvoked` for
      `exit`. Spawning the task does **not** require user-space
      ELF loading: a kernel thread that issues `syscall` from
      kernel CPL=0 is rejected by the trampoline (AGENTS.md §5.4
      fail-closed), so this test instead invokes
      `Dispatcher::dispatch` directly through a dedicated
      `test-hook` entry exposed only when the bin crate is built
      with the `test-hooks` Cargo feature. The hook is gated off
      by default and `cargo deny check` rejects accidental
      release builds that enable it.

- [ ] **(f7)** PLAN.md update: tick (f1)..(f6), refresh the
      Stage 3a status block to note Stage 2.7 follow-up is
      `complete`, and refresh the Stage 2 evidence tail with a
      fresh `cargo xtask ci` quote.

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

### Stage 2.7 follow-up status — partial

Sub-items (f1)..(f5) have landed; (f6)..(f7) remain. Commits on
`master`:

- `c93e823` — kernel/sched: per-CPU current-task slot (f1).
- `fcfb5fc` — kernel/sec: TaskId→TaskCapabilities CapTable
  registry (f2).
- `4497106` — kernel/core: production `SyscallHandlers` impl +
  `KernelArch::monotonic_ns` (f3).
- `eca9e89` — kernel/core: `DispatchCallbackSlot` + `Phase::Syscall`
  + `KernelDispatchHook` + `KernelState` wiring (f4).
- `45c21c3` — kernel/rustos-kernel: `production_dispatch` swap +
  `encode_result` + `DISPATCH_SLOT` install through `BootInfo` (f5).

`cargo xtask ci` is green at HEAD (`45c21c3`), running through the
Stage 2 evidence pipeline above unchanged. The next session picks
up at (f6): the `test-hooks`-gated QEMU integration test that
drives `Dispatcher::dispatch` directly for `cap_query` + `exit`
and observes the resulting `SyscallInvoked` audit records.
Detailed continuation prompt remains at
[`.junie/next-session-prompt.md`](./.junie/next-session-prompt.md).

The Stage 3a status block above (`Status: complete`) is unchanged
— Stage 3a's (a)..(d1) deliverables are done; Stage 2.7 follow-up
is its own thread and is tracked here, not by re-opening Stage 3a.

---

## Stage 4 — Driver Framework and First Drivers

**Dependencies:** Stage 2 + at least one Stage 3 sub-stage.

**Deliverables**
- `lib/abi/src/driver/` driver traits per class
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

**Tests**
- POSIX FS test suite (`pjdfstest`-equivalent) run under QEMU.
- ACL + capability gate tests: a user without `CAP_AUDIT_READ` cannot read
  a file marked as such, even with mode 0644.
- Crash-consistency tests for `rustfs` journal.

**Docs**
- `docs/src/filesystem/{overview,rustfs,ext4,fat32,permissions}.md`.

---

## Stage 6 — Userland Foundations

**Dependencies:** Stages 2–5 sufficient for at least one platform.

**Deliverables**
- `userland/init` (PID 1): service manager, dependency-ordered start,
  reaper, capability granting from manifests.
- `userland/shell`: POSIX-ish shell with job control and a small builtin set.
- `userland/login`: text login that authenticates against `kernel/sec` and
  spawns a shell or a graphical session. Always starts in text mode; offers
  graphical mode only when a display driver and `userland/wm` are available.
- Core CLI utilities (`ls`, `cp`, `mv`, `rm`, `cat`, `ps`, `mount`,
  `chmod`, `chown`, `useradd`, `groupadd`, `setcap`, `getcap`).
  Each utility is its own small crate under `userland/apps/`.

**Tests**
- Integration tests: boot to login, log in, run each utility, exercise
  permission denials.

**Docs**
- `docs/src/userland/{init,login,shell,utilities}.md`.

---

## Stage 7 — Graphics, Window Manager, Iconbar

**Dependencies:** Stage 6 + a display driver from Stage 4.

**Deliverables**
- `userland/wm`: compositing window manager. Per-window surfaces, damage
  tracking, GPU acceleration where a driver exposes it, software fallback
  otherwise.
- `userland/iconbar`: RISC OS-style iconbar with pinned app slots, mounted
  filesystem icons, and a status area.
- Default theme + cursor set.
- A handful of default apps under `userland/apps/`: filer, text editor,
  terminal emulator, settings panel (users, groups, permissions, caps).

**Tests**
- Headless compositor tests using a virtual framebuffer.
- Input routing tests (focus, drag-and-drop save model).

**Docs**
- `docs/src/desktop/{wm,iconbar,apps,theming}.md`.

---

## Stage 8 — Installer and Image Builders

**Dependencies:** Stages 5, 6 (and 7 for the graphical installer path).

**Deliverables**
- `userland/installer` with text and graphical front-ends sharing one core
  library. Functions per `AGENTS.md` §11.
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
- **No duplication:** code reviewers reject duplication; refactor into
  `lib/` instead.

---

## Assignment Notes for Task Dispatchers

When handing a stage to an implementing agent, the task brief **must**:

1. Reference this `PLAN.md` and the `AGENTS.md` charter explicitly.
2. List the stage's deliverables, tests, and docs verbatim.
3. State the dependencies that are already satisfied.
4. Forbid stubs, `todo!()`, ignored tests, and `#[allow(...)]` without
   justification.
5. Require the agent to quote actual `cargo xtask test` output on completion.

A stage delivered without the above is to be returned for rework, regardless
of how much code was produced.
