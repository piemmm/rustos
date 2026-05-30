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
- `LICENSE` (GPL-3.0-only), `README.md`, `AGENTS.md`, `PLAN.md` are
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
  walker iterates the boot DTB through `rustos_util::dtb` (a new
  shared parser promoted into `lib/util` once a second caller
  materialised per `AGENTS.md` §2.3 / §6 — the consumers today
  are this driver and the future platform-discovery code that
  reads the same boot blob). Tests: 12 host-side unit tests for
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
  drivers (`drivers/display/vesa`, `drivers/display/framebuffer`,
  `drivers/input/ps2`) remain outstanding per the Stage 4
  deliverable list above; packed virtqueues (virtio 1.1 §2.7) are
  a Stage 5 follow-up documented in `docs/src/drivers/virtio.md`.
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
  clean. **Deferred (the gate).** The crate is **not** enrolled in
  `cargo xtask test --qemu`: a ~30% intermittent single-CPU hang in the
  MSI completion-wait path (guest spins `IF=0` near `IrqTable::fire`
  right after the device's first completion interrupt) is still under
  investigation, and `AGENTS.md` §7 forbids gating CI on a flaky test.
  The `disk_sectors` plumbing in `tools/xtask/src/commands/qemu_tests.rs`
  stays so re-enrolment is a one-line `QemuTest { … disk_sectors:
  Some(2048) }` once the hang is root-caused — see
  `.junie/next-session-prompt.md`.
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
  index behind a writer-preference `kernel/sync::RwLock` mirroring
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
  [`docs/src/security/irq.md`](src/security/irq.md) locks down the
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

---

## Stage 7 — Graphics, Window Manager, Iconbar

**Dependencies:** Stage 6 + a display driver from Stage 4.

**Deliverables**
- `userland/gui/wm`: compositing window manager. Per-window surfaces, damage
  tracking, GPU acceleration where a driver exposes it, software fallback
  otherwise.
- `userland/gui/iconbar`: RISC OS-style iconbar with pinned app slots, mounted
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
- `userland/system/installer` with text and graphical front-ends sharing one core
  library. Functions per `AGENTS.md` §11 and lays out the filesystem per
  `AGENTS.md` §16: exactly `/System`, `/Users`, `/Apps`, `/Storage`; no
  legacy POSIX top-level directories; mount flags as specified in §11.3
  and §16.3; expert mode refuses any reserved name.
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
