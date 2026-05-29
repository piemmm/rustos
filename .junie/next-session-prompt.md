# Next session — Stage 4.D Items 2–6 (virtio end-to-end on real hardware)

## Where we are

Item 0-tail of the previous next-session prompt landed in the
preceding session and is recorded in `PLAN.md` Stage 4.D under
"Item 0-tail — `KernelVirtioHost` plumbing into
`userland/system/drvhost`, *complete*". Two structural deliverables
came out of it that the rest of this prompt builds on:

1. **ABI re-home.** `PoolId`, `SlabFreeFn`, `DmaSlab` and the
   `VirtioHost` trait now live in `lib/abi/src/driver/{dma.rs,
   virtio.rs}`. `drivers/bus/virtio` re-exports them so every
   existing import site is unchanged. The previous next-session
   prompt's "add `DriverHost::virtio_host(&mut self) -> &dyn
   VirtioHost` directly in `lib/abi`" instruction was reshaped to
   `DriverHost::virtio_host(&self) -> Option<&dyn VirtioHost>`
   (defaulted to `None`) because the frozen `register(host: &dyn
   DriverHost) -> Result<DriverHandle, DriverError>` entry point
   per `AGENTS.md` §8 is immutable, and `VirtioHost`'s own methods
   already use `&self` plus interior mutability.

2. **Drvhost factory seam.** `userland/system/drvhost::host` has a
   `VirtioHostFactory` trait, a `HostConfig::virtio_host_factory:
   Option<&'h dyn VirtioHostFactory>` slot, and a `LoadedHostView`
   that borrows the factory-minted host for the duration of a
   single `register()` call. The factory abstraction was chosen
   deliberately to keep drvhost free of `kernel/*` deps and free of
   the `KernelVirtioHost::<P: PageTableOps, S: Sink>` generics in
   its production build. The kernel binary supplies an impl whose
   internals do mention those generics (Item 2 of this prompt
   wires that impl alongside the IRQ plumbing).

Baseline as of the start of this session: `cargo test --workspace
--lib --exclude rustos-kernel-arch-*` → 663 passing; `cargo test
-p rustos-drvhost` → 19 lib + 15 integration; `cargo test -p
rustos-drv-bus-virtio` default → 41 passing, `--features
kernel-host` → 41 passing; `cargo clippy -p rustos-abi -p
rustos-drv-bus-virtio -p rustos-drvhost --all-targets --all-features
-- -D warnings` and `cargo fmt --check` on the touched crates are
clean. The pinned toolchain is `nightly-2026-05-27` per
`rust-toolchain.toml`.

## Reading list

- `AGENTS.md` (binding).
- `PLAN.md` Stage 4 status block — in particular the new "Item
  0-tail" paragraph and the "Item 0" / "Item 0a" paragraphs it
  builds on.
- This file.
- `.junie/next-session-prompt.prev.md` for the historical Items 2–6
  text that this prompt supersedes.
- `lib/abi/src/driver/{dma.rs, virtio.rs}` for the re-homed ABI
  types and the new `DriverHost::virtio_host` accessor.
- `userland/system/drvhost/src/host.rs` for `VirtioHostFactory` and
  the per-driver virtio plumbing.
- `drivers/bus/virtio/src/kernel_host.rs` for `KernelVirtioHost`
  (the factory implementation Item 2 wires in the kernel binary).
- `drivers/bus/pci/src/lib.rs`, `drivers/bus/mmio/src/lib.rs` (bus
  driver side of Item 3).
- `lib/abi/src/{capability.rs, syscalls.rs}` and
  `kernel/syscall/src/table.rs` for Item 2 (the `irq_bind` /
  `irq_wait` syscall pair).

## What needs doing

### Item 2 — IRQ routing into user-space drivers

The `VirtioHost::notify_wait` body inside `KernelVirtioHost` is
still the polled cooperative shim from `MockHost` (it pushes the
`queue_index` into an in-process log). The kernel side that
replaces it lands in this Item.

- New `CapabilityId::IRQ_BIND = 11` in `lib/abi/src/capability.rs`,
  mirrored in `kernel/sec::is_known_capability` and the
  audit-frozen-id tests.
- Syscalls `irq_bind(line: u32) -> IrqHandle` and
  `irq_wait(handle, timeout_ns: u64) -> Result<(), Errno>`. Update
  both `lib/abi/src/syscalls.rs` and `kernel/syscall/src/table.rs`
  in the same commit (`cargo xtask abi-check` enforces this).
- Kernel-side IRQ table + wait queue under `kernel/sched` (one
  wait queue per `IrqHandle`); reuse the existing scheduler primitives.
- Plumb the handle through `KernelVirtioHost::notify_wait` so the
  polled cooperative shim is replaced by a real wait. The polled
  log accessor is retained only on `MockHost`; the production path
  blocks on `IrqHandle`.
- New docs page `docs/src/security/irq.md` describing the wake-up
  contract; update `docs/src/architecture/kernel.md` to reference
  it.
- Tests: an in-tree mock-device QEMU integration test that arms
  an IRQ from a small in-tree device and verifies wake-up + masking.

**Kernel-side factory.** Once the IRQ plumbing exists, wire a
`VirtioHostFactory` impl in the production kernel binary
(`kernel/src/main.rs`-ish surface — locate the existing per-process
`DmaPool` carve point) that mints a fresh `KernelVirtioHost`
per loaded driver and passes it through `HostConfig::virtio_host_factory`.
The drvhost seam already accepts it; the work is the kernel-side
implementation. **Do not** add a `kernel-host` feature to
`userland/system/drvhost` itself — the factory abstraction is
designed so drvhost stays free of `kernel/*` deps.

### Item 3 — Bus-handle hand-off from `drivers/bus/{pci,mmio}`

- Extend the `PciBackend` / `MmioBackend` constructors in
  `drivers/bus/virtio` to receive a capability-checked register
  window rather than the bare identification tuple they carry
  today.
- The PCI and MMIO bus drivers obtain the window from the kernel
  via the DMA / future MMIO-map facility (the *kernel* allocates
  the window; the bus driver does not synthesise pointers).
- Per-bus unit tests with mock register windows; a QEMU
  integration test that walks PCI / DTB and hands a working
  window through to the virtio transport.
- Update `docs/src/drivers/bus.md` with the hand-off sequence and
  the capability flow.

### Item 4 — QEMU integration tests

Once Items 2 + 3 are in place:

- `tests/integration/virtio_blk_pci_x86_64` — boots the kernel +
  driver host + signed `.rxe`, attaches `virtio-blk` to a backing
  qcow2, reads sector 0 (planted by `tools/qemu`), writes a known
  pattern to sector 1, reads it back, verifies checksum.
- `tests/integration/virtio_blk_mmio_riscv64` — same against
  `qemu-system-riscv64 -M virt` with `virtio-blk-device`.
- `tests/integration/virtio_net_pci_x86_64` and
  `tests/integration/virtio_net_mmio_riscv64` — ARP + ICMP echo
  round-trip against `qemu user net`'s built-in DHCP/ARP/ICMP
  responder. Depends on Item 5.
- Add an unload → reload → reuse test for each driver. The
  drvhost `VirtioHostFactory` is the seam through which a fresh
  `KernelVirtioHost` (and therefore a fresh per-driver `DmaPool`)
  is minted on each load.

### Item 5 — Userland ARP / IP / ICMP responder

The virtio-net QEMU integration tests need a small userland stack:

- New crate `userland/net/icmp/` implementing only ARP request +
  reply, IP + ICMP echo, and a minimal main loop sitting on top
  of the `Net` trait.
- Out of scope: TCP, UDP, IPv6, routing — those are Stage 6 work.

### Item 6 — Acceptance gate

After Items 2–5 land:

- Run `cargo xtask ci` and paste verbatim output in the PR body.
- Run `cargo xtask test` and paste verbatim output.
- Confirm coverage ≥ 75 % on each new crate per `AGENTS.md` §7
  (`userland/net/icmp`, the four new QEMU integration crates).
- Confirm `kernel/sec` and `kernel/ipc` coverage remain ≥ 95 %
  after the IRQ-plumbing additions (the bar set by `AGENTS.md` §7
  for security-critical kernel crates).

## Toolchain note

The pinned `nightly-2026-05-27` is required for
`kernel/arch/x86_64` (`#[unsafe(naked)]`, inline-const). On
systems without `rustup` on PATH it ships under
`~/.rustup/toolchains/nightly-2026-05-27-<triple>/bin`; export
that on PATH before invoking `cargo`. The preceding session
validated the baseline on that toolchain.

## Assumptions for the next session to confirm at the top of the PR body

1. `lib/abi::driver::{DmaSlab, PoolId, SlabFreeFn, VirtioHost}` is
   the canonical home for the host↔driver virtio ABI seam.
   `drivers/bus/virtio` re-exports them; **do not** define a
   parallel set in the bus crate. If a different shape is
   required, record the rationale in `PLAN.md` and ship its tests
   + docs in the same commit.
2. `kernel/sec::dma::{alloc_dma, free_dma}` remain the only
   blessed capability-checked entry points; the bus + virtio
   drivers must not call `DmaPool` directly.
3. The `DriverHost::virtio_host(&self) -> Option<&dyn VirtioHost>`
   accessor is an `abi-v1` internal addition (not user-facing);
   the public driver entry point `pub fn register(host: &dyn
   DriverHost) -> Result<DriverHandle, DriverError>` per
   `AGENTS.md` §8 is unchanged.
4. `userland/system/drvhost::VirtioHostFactory` is the seam at
   which the kernel binary supplies a real `KernelVirtioHost`-backed
   factory. Drvhost stays free of `kernel/*` deps; the factory
   impl lives in the kernel binary (or in a thin kernel-side
   adapter crate). Do **not** add a `kernel-host` feature to
   `userland/system/drvhost` itself.
5. `DmaSlab` is `Send` (no `Sync`); the kernel host must not call
   `as_bytes_mut` from two threads on the same slab.
6. `KernelVirtioHost::notify_wait` will lose its polled body in
   Item 2 — the implementation blocks on an `IrqHandle` instead
   of pushing to the in-process notify log. The log accessor is
   retained only for `MockHost`.
