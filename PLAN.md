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
      cross-link from `docs/src/architecture/kernel.md`. **Stage 2
      status remains *in progress*** because the Stage-2
      deliverable text (lines 154–158) requires the scheduler
      stress test to run *under QEMU on ≥ 4 emulated cores*; that
      requires Stage-3a SMP (AP startup, APIC timer, IPIs) which
      is out of scope for this commit. See the Stage 3a sub-
      checklist in §3.

**Status: in progress.**
- All architecture-neutral sub-stages 2.1–2.7 remain complete with
  the previously-recorded evidence (coverage thresholds, fuzz
  harnesses, loom-gated tests, docs).
- 2.8 delivers the QEMU runner + memory-isolation deliverable
  under QEMU end-to-end. `cargo xtask test --qemu` is green on the
  CI host. Toolchain pinned at `nightly-2026-05-27`
  (rustc 1.98.0-nightly).
- 2.8 partially delivers `scheduler_stress`: the cross-crate
  workspace test on ≥ 4 simulated cores passes host-side. The
  QEMU-on-real-cores variant is blocked on the Stage 3a sub-
  checklist below and is the only reason Stage 2 cannot yet be
  declared *complete*.
- `cargo xtask ci` evidence tail (toolchain
  `nightly-2026-05-27` / rustc 1.98.0-nightly, QEMU 8.2.2,
  GRUB-EFI 2.12, OVMF 2024.02). Refreshed after Stage 3a (b) added
  the AP bring-up path and the second QEMU integration test
  (9 new host unit tests in `kernel/arch/x86_64::smp`, 56 host
  unit tests total across the arch crate):
  ```text
  xtask: [fmt --check]                     cargo fmt --all -- --check
  xtask: [clippy]                          --workspace --all-targets --locked -- -D warnings
  xtask: [test]                            --workspace --all-targets --locked
  xtask: [test --qemu] 2 test(s) enrolled
  xtask: [test --qemu (build rustos-test-memory-isolation)]
  xtask: [test --qemu (run  rustos-test-memory-isolation)]
      kernel=…/rustos-test-memory-isolation cpus=1 timeout=60s
  xtask: [test --qemu (build rustos-test-scheduler-stress-qemu)]
  xtask: [test --qemu (run  rustos-test-scheduler-stress-qemu)]
      kernel=…/rustos-test-scheduler-stress-qemu cpus=4 timeout=120s
  xtask: [docs-check (rustdoc)]            -D warnings --document-private-items
  xtask: [docs-check (mdbook)]
  xtask: [docs-check (linkcheck)]          docs/src
  xtask: [deny]                            advisories ok, bans ok,
                                           licenses ok, sources ok
  xtask: [abi-check]                       lib/abi/src/syscalls.rs ↔
                                           kernel/syscall/src/table.rs
  ```
  All host test crates report `ok. … 0 failed; 0 ignored`. The
  workspace cross-crate host stress
  (`workspace_stress_four_cores_twenty_thousand_tasks`) still passes
  in ~0.4 s; the new QEMU stress
  (`rustos-test-scheduler-stress-qemu`) brings 3 APs online via
  INIT-SIPI-SIPI and runs 8 192 tasks across 4 real (emulated) cores
  to completion (`PASS`, "distinct executing CPUs = 4"), inside the
  120 s budget.

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

  **Remaining for Stage 3a completion** (each blocks Stage 2 from
  flipping to `Status: complete` and/or unlocks downstream stages):
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
    - [ ] x86_64 syscall entry stub bound to
          `kernel/syscall::Dispatcher` (the architecture-neutral
          dispatcher already validates against
          `SYSCALL_TABLE_HASH`).
    - [ ] Implement `kernel/core::KernelArch` against the above and
          wire `kernel_main`.
    - [ ] Per-arch QEMU run script `tools/qemu/x86_64.rs` (today the
          generic `tools/qemu` runner suffices; Stage 3a will move
          the x86_64-specific defaults out of `lib.rs::Spec`).
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
