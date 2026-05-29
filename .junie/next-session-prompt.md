# Next session — Stage 4.D Items 2–6 (virtio end-to-end on real hardware)

## Where we are

Item 0 of the previous next-session prompt landed in the
immediately preceding session: the real, capability-checked
`KernelVirtioHost<'a, P: PageTableOps, S: Sink + ?Sized>` now
sits alongside `MockHost` in
`drivers/bus/virtio/src/kernel_host.rs`, gated behind the new
`kernel-host` Cargo feature (off by default). It wraps a
borrowed `&'a mut DmaPool<'a, P>`, the calling task's
`&'a TaskCapabilities`, an audit `&'a S`, a fresh `PoolId`, a
monotonic slot counter, and a `RefCell<BTreeMap<usize,
DmaBuffer>>` live-slot table. `alloc_dma_zeroed` routes through
`kernel/sec::dma::alloc_dma`, mints a `DmaSlab` via
`DmaSlab::from_pool` with a generic `slab_free_shim::<P, S>`,
and the shim routes the buffer back through
`kernel/sec::dma::free_dma` on slab drop. Seven new unit tests
land alongside the type (zero-initialised slab + audit emit,
drop routes through `free_dma`, `MEM_DMA` refusal returns
`PermissionDenied`, zero-size short-circuit, two simultaneous
disjoint slabs, `notify_wait` records queue index, oversize
collapses to `LengthOutOfRange`). The `DriverHost` trait was
**not** extended with a `dma_pool` accessor: there is no
in-tree `.rxe` driver yet that would consume one, so adding it
ahead of a consumer would violate `AGENTS.md` §2.3 (no bloat).
The drvhost ↔ `KernelVirtioHost` plumbing therefore lands in
this session, alongside the first in-tree `.rxe` consumer
(virtio-blk / virtio-net) that needs it.

Baseline as of the start of this session: `cargo test
--workspace --lib --exclude rustos-kernel-arch-*` → 663 passing,
0 failing on the pinned `nightly-2026-05-27`. `cargo clippy ...
-D warnings` and `cargo fmt --check` are clean across the
touched crates.

## Reading list

- `AGENTS.md` (binding).
- `PLAN.md` Stage 4 status block — in particular the new
  Item 0 ("complete") paragraph and the Item 0a / Item 1
  paragraphs it builds on.
- This file.
- `drivers/bus/virtio/src/kernel_host.rs` for the
  `KernelVirtioHost` surface introduced in the previous
  session.
- `drivers/bus/virtio/src/{dma.rs, host.rs, queue.rs}` for the
  owned-slab shape (`from_leaked` / `from_pool` /
  `SlabFreeFn`).
- `kernel/mem/src/dma.rs::slot_base`,
  `kernel/sec/src/dma.rs::{alloc_dma, free_dma}`.
- `userland/system/drvhost/src/host.rs` (the load-side surface
  Items 0-tail and 4 extend).
- `drivers/bus/pci/src/lib.rs`, `drivers/bus/mmio/src/lib.rs`
  (bus driver side of Item 3).
- `lib/abi/src/{capability.rs, syscalls.rs}` and
  `kernel/syscall/src/table.rs` for Item 2 (the `irq_bind` /
  `irq_wait` syscall pair).

## What needs doing

### Item 0-tail — Thread `KernelVirtioHost` through `userland/system/drvhost`

This is the half of the previous prompt's Item 0 that was
deliberately deferred until an in-tree consumer existed.
Land it now alongside Item 4 (the QEMU virtio-blk / -net
integration tests).

- Extend the driver-host's per-driver context with a borrowed
  `&'a mut DmaPool<'a, P>` (one pool per loaded driver module,
  carved from the kernel allocator for that process).
- The per-driver context constructs a `KernelVirtioHost` and
  hands it to the driver via a new internal trait method on
  `DriverHost` — `dma_pool(&mut self) -> &mut DmaPool<P>` is
  the wrong shape (it leaks `kernel/mem` types through
  `lib/abi`); use `virtio_host(&mut self) -> &dyn VirtioHost`
  instead so the cross-arch boundary stays at the trait
  defined in `drivers/bus/virtio`. Justify the trait
  extension as an `abi-v1` internal addition in `PLAN.md`
  before adding it.
- Update `userland/system/drvhost` unit tests + the
  `drvhost_qemu` integration to exercise the real path.
- The shipped `MockHost` / `MockTransport` test seam stays
  in place: the new path is *additional*, not a replacement.

### Item 2 — IRQ routing into user-space drivers

- New `CapabilityId::IRQ_BIND = 11` in `lib/abi`, mirrored in
  `kernel/sec::is_known_capability` and the audit-frozen-id
  tests.
- Syscalls `irq_bind(line: u32) -> IrqHandle` and
  `irq_wait(handle, timeout) -> Result<(), Errno>`. Update
  both `lib/abi/src/syscalls.rs` and
  `kernel/syscall/src/table.rs` in the same commit
  (`cargo xtask abi-check` enforces this).
- The driver host plumbs the handle through
  `VirtioHost::notify_wait` so the polled cooperative shim
  in the virtio crate is replaced by a real wait. Document
  the wakeup contract in `docs/src/architecture/kernel.md`
  and a new `docs/src/security/irq.md`.
- Tests: a QEMU integration test that arms an IRQ from a
  small in-tree mock device and verifies wake-up + masking.

### Item 3 — Bus-handle hand-off from `drivers/bus/{pci,mmio}`

- Extend the `PciBackend` / `MmioBackend` constructors in
  `drivers/bus/virtio` to receive a capability-checked
  register window rather than the bare identification
  tuple they carry today.
- The PCI and MMIO bus drivers obtain the window from the
  kernel via the DMA / future MMIO-map facility (the
  *kernel* allocates the window; the bus driver does not
  synthesise pointers).
- Per-bus unit tests with mock register windows; a QEMU
  integration test that walks PCI / DTB and hands a working
  window through to the virtio transport.
- Update `docs/src/drivers/bus.md` with the hand-off
  sequence and capability flow.

### Item 4 — QEMU integration tests

Once Items 0-tail, 2, 3 are in place:

- `tests/integration/virtio_blk_pci_x86_64` — boots the
  kernel + driver host + signed `.rxe`, attaches
  `virtio-blk` to a backing qcow2, reads sector 0 (planted
  by `tools/qemu`), writes a known pattern to sector 1,
  reads it back, verifies checksum.
- `tests/integration/virtio_blk_mmio_riscv64` — same against
  `qemu-system-riscv64 -M virt` with `virtio-blk-device`.
- `tests/integration/virtio_net_pci_x86_64` and
  `tests/integration/virtio_net_mmio_riscv64` — ARP + ICMP
  echo round-trip against `qemu user net`'s built-in
  DHCP/ARP/ICMP responder. Depends on Item 5.
- Add an unload → reload → reuse test for each driver.

### Item 5 — Userland ARP / IP / ICMP responder

The virtio-net QEMU integration tests need a small userland
stack:

- New crate `userland/net/icmp/` implementing only ARP
  request + reply, IP + ICMP echo, and a minimal main loop
  sitting on top of the `Net` trait.
- Out of scope: TCP, UDP, IPv6, routing — those are Stage 6
  work.

### Item 6 — Acceptance gate

After Items 0-tail, 2–5 land:

- Run `cargo xtask ci` and paste verbatim output in the PR
  body.
- Run `cargo xtask test` and paste verbatim output.
- Confirm coverage ≥ 75 % on each new crate per
  `AGENTS.md` §7.

## Toolchain note

The pinned `nightly-2026-05-27` is required for
`kernel/arch/x86_64` (`#[unsafe(naked)]`, inline-const). On
systems without `rustup` on PATH it ships under
`~/.rustup/toolchains/nightly-2026-05-27-<triple>/bin`; export
that on PATH before invoking `cargo`. The preceding session
validated `cargo test --workspace --lib --exclude
rustos-kernel-arch-*` → 663 passing on that toolchain.

## Assumptions for the next session to confirm at the top of the PR body

1. The `KernelVirtioHost` minted in `drivers/bus/virtio` is
   the blessed construction site for any per-driver DMA host;
   the `MockHost` continues to be the test seam. If a
   different shape is required, the next-session author
   records the rationale in `PLAN.md` and ships its tests +
   docs in the same commit.
2. `kernel/sec::dma::{alloc_dma, free_dma}` remain the only
   blessed capability-checked entry points; the bus + virtio
   drivers must not call `DmaPool` directly.
3. The `DriverHost` trait extension introduced by Item 0-tail
   is an `abi-v1` internal interface (not user-facing); the
   public driver entry point (`pub fn register(host: &dyn
   DriverHost) -> Result<DriverHandle, DriverError>` per
   `AGENTS.md` §8) is unchanged.
4. `DmaSlab` exposes `Send` (no `Sync`); the kernel host must
   not call `as_bytes_mut` from two threads on the same slab.
5. `KernelVirtioHost::notify_wait` will lose its polled body
   in this session — once Item 2 lands, the implementation
   blocks on an `IrqHandle` instead of pushing to the
   in-process notify log. The log accessor is retained only
   for `MockHost`.
