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
