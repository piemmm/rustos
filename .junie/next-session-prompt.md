# Next session — Stage 4.D Items 0, 2–6 (virtio end-to-end on real hardware)

## Where we are

Item 0a of the previous next-session prompt landed in the immediately
preceding session: the driver-side `DmaRegion<'a>` borrowed view has
been replaced by an owned

```rust
struct DmaSlab {
    phys: u64,
    ptr: NonNull<u8>,
    len: usize,
    pool_id: PoolId,
    slot: usize,
    /* type-erased free shim */
}
```

in `drivers/bus/virtio/src/dma.rs`, `BounceBuffer` now wraps a
`DmaSlab`, the `VirtioHost` trait returns `Result<DmaSlab,
DriverError>` (no lifetime), and `SplitQueue` stores three owned
`DmaSlab`s. The pool exposes the single new accessor
`DmaPool::slot_base(&self, &DmaBuffer) -> Result<NonNull<u8>,
DmaError>` (`kernel/mem/src/dma.rs`); the `MockHost` still uses
`Box::leak` storage but mints slabs with `PoolId::MOCK`, a monotonic
slot counter, and a `None` free shim. Four new `DmaSlab` tests in
`drivers/bus/virtio/src/dma.rs` cover the round-trip, three
simultaneous disjoint writes, drop-frees-pool, and pool-id rejection
across pools. Two new `slot_base*` tests have landed in
`kernel/mem/src/dma/tests.rs`. Docs:
`docs/src/drivers/virtio.md` (new "DMA ownership model" section) and
`docs/src/architecture/memory.md` (new §5.1 "Slab hand-off to
user-space drivers").

Baseline as of the start of this session: `cargo test --workspace
--lib --exclude rustos-kernel-arch-*` → 656 passing, 0 failing on
the pinned `nightly-2026-05-27`. `cargo clippy ... -D warnings` and
`cargo fmt --check` are clean across the touched crates.

## Reading list

- `AGENTS.md` (binding).
- `PLAN.md` Stage 4 status block — in particular the new Item 0a
  ("complete") paragraph and the original Item 0 paragraph it
  supersedes.
- This file.
- `drivers/bus/virtio/src/{dma.rs, host.rs, queue.rs}` for the new
  owned-slab shape (`from_leaked` / `from_pool` / `SlabFreeFn`).
- `kernel/mem/src/dma.rs::slot_base`, `kernel/sec/src/dma.rs::{alloc_dma,
  free_dma}`.
- `userland/system/drvhost/src/host.rs` (the load-side surface that
  Item 0 extends).
- `drivers/bus/pci/src/lib.rs`, `drivers/bus/mmio/src/lib.rs` (bus
  driver side of Item 3).

## What needs doing

### Item 0 — Thread `DmaPool` through `userland/system/drvhost`

- Extend the driver-host's per-driver context with a borrowed
  `&mut DmaPool<P>` (one pool per loaded driver module, carved from
  the kernel allocator for that process).
- Implement a real `KernelVirtioHost<'a, P>` in
  `drivers/bus/virtio` (alongside `MockHost`) backed by the
  per-driver pool. `alloc_dma_zeroed` routes through
  `kernel_sec::alloc_dma(pool, caller_caps, size, audit)?`, then
  constructs the returned `DmaSlab` from
  `DmaPool::slot_base(buf)`, the `buf.phys()`, and a
  `SlabFreeFn`-compatible shim that closes over the pool pointer
  and routes the drop through `kernel_sec::free_dma`. The pool
  pointer used at construction must remain valid until the slab's
  drop; documented in the `// SAFETY:` block on
  `DmaSlab::from_pool`.
- The `DriverHost` trait (currently `has_capability` + `kind`)
  gains a `dma_pool(&mut self) -> &mut DmaPool<P>` accessor; the
  driver `register()` entry point receives the host context that
  owns it.
- Update `userland/system/drvhost` unit tests + the `drvhost_qemu`
  integration to exercise the real path.
- The shipped `MockHost` / `MockTransport` test seam stays in
  place: the new path is *additional*, not a replacement.

This is a precondition for Items 2–4 working end-to-end.

### Item 2 — IRQ routing into user-space drivers

- New `CapabilityId::IRQ_BIND = 11` in `lib/abi`, mirrored in
  `kernel/sec::is_known_capability` and the audit-frozen-id tests.
- Syscalls `irq_bind(line: u32) -> IrqHandle` and
  `irq_wait(handle, timeout) -> Result<(), Errno>`. Update both
  `lib/abi/src/syscalls.rs` and `kernel/syscall/src/table.rs` in
  the same commit (`cargo xtask abi-check` enforces this).
- The driver host plumbs the handle through
  `VirtioHost::notify_wait` so the polled cooperative shim in the
  virtio crate is replaced by a real wait. Document the wakeup
  contract in `docs/src/architecture/kernel.md` and a new
  `docs/src/security/irq.md`.
- Tests: a QEMU integration test that arms an IRQ from a small
  in-tree mock device and verifies wake-up + masking.

### Item 3 — Bus-handle hand-off from `drivers/bus/{pci,mmio}`

- Extend the `PciBackend` / `MmioBackend` constructors in
  `drivers/bus/virtio` to receive a capability-checked register
  window rather than the bare identification tuple they carry
  today.
- The PCI and MMIO bus drivers obtain the window from the kernel
  via the DMA / future MMIO-map facility (the *kernel* allocates
  the window; the bus driver does not synthesise pointers).
- Per-bus unit tests with mock register windows; a QEMU
  integration test that walks PCI / DTB and hands a working window
  through to the virtio transport.
- Update `docs/src/drivers/bus.md` with the hand-off sequence and
  capability flow.

### Item 4 — QEMU integration tests

Once Items 0, 2, 3 are in place:

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
- Add an unload → reload → reuse test for each driver.

### Item 5 — Userland ARP / IP / ICMP responder

The virtio-net QEMU integration tests need a small userland stack:

- New crate `userland/net/icmp/` implementing only ARP request +
  reply, IP + ICMP echo, and a minimal main loop sitting on top of
  the `Net` trait.
- Out of scope: TCP, UDP, IPv6, routing — those are Stage 6 work.

### Item 6 — Acceptance gate

After Items 0, 2–5 land:

- Run `cargo xtask ci` and paste verbatim output in the PR body.
- Run `cargo xtask test` and paste verbatim output.
- Confirm coverage ≥ 75 % on each new crate per `AGENTS.md` §7.

## Toolchain note

The pinned `nightly-2026-05-27` is required for `kernel/arch/x86_64`
(`#[unsafe(naked)]`, inline-const). On systems without `rustup` on
PATH it ships under
`~/.rustup/toolchains/nightly-2026-05-27-<triple>/bin`; export that
on PATH before invoking `cargo`. The preceding session validated
`cargo test --workspace --lib --exclude rustos-kernel-arch-*` → 656
passing on that toolchain.

## Assumptions for the next session to confirm at the top of the PR body

1. `DmaSlab::from_pool` is the blessed construction site for any
   real `KernelVirtioHost`; the `MockHost` continues to use
   `from_leaked`. If a different shape is required, the
   next-session author records the rationale in `PLAN.md` and
   ships its tests + docs in the same commit.
2. `kernel/sec::dma::{alloc_dma, free_dma}` remain the only
   blessed capability-checked entry points; the bus + virtio
   drivers must not call `DmaPool` directly.
3. The `DriverHost` trait extension introduced by Item 0 is an
   `abi-v1` internal interface (not user-facing); the public
   driver entry point (`pub fn register(host: &dyn DriverHost) ->
   Result<DriverHandle, DriverError>` per `AGENTS.md` §8) is
   unchanged.
4. `DmaSlab` exposes a `Send` (no `Sync`) impl; the kernel host
   must not call `as_bytes_mut` from two threads on the same slab.
