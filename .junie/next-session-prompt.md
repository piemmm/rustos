# Next session — Stage 4.D Items 0a, 0, 2–6 (virtio end-to-end on real hardware)

## Where we are

Item 1 of the previous next-session prompt landed two sessions ago (kernel
per-process-heap DMA allocator + capability gate + audit catalogue rows). The
preceding session — the one that produced *this* file — surveyed Items 0
and 2–6 against the post-Item-1 codebase, ran the baseline (`cargo test
--workspace --lib`, minus the four `kernel/arch/*` targets that need a real
boot environment, on the pinned `nightly-2026-05-27`) and confirmed it at
**650 passing, 0 failing**, and discovered an unresolved API-shape conflict
between `kernel/mem::DmaPool` and `drivers/bus/virtio::VirtioHost` that
blocks Item 0 as previously written. The conflict is documented in detail
in `PLAN.md` under the new Stage 4.D follow-up "Item 0 — `DmaPool` ↔
`VirtioHost` API shape, *unresolved*" bullet; the work below has been
re-sequenced so Item 0a (the resolution) lands first.

No source files were touched in the surveying session — the API choice is a
versioned-interface decision (`AGENTS.md` §2.4) and the prior session's
Assumption 1 explicitly routes such cases through `PLAN.md` rather than
in-place mutation.

## Reading list

- `AGENTS.md` (binding).
- `PLAN.md` Stage 4 status block — in particular the *two* Item-1 and
  Item-0 paragraphs at the end.
- This file.
- `kernel/mem/src/dma.rs`, `kernel/sec/src/dma.rs` (the seam Items 2 and 3
  plug into).
- `drivers/bus/virtio/src/{host.rs, dma.rs, queue.rs, transport.rs}` (the
  consumer side of the kernel API; `SplitQueue::new` is the canonical
  three-simultaneous-region call site).
- `drivers/storage/virtio_blk/src/lib.rs::submit` and
  `drivers/network/virtio_net/src/lib.rs::transmit` (additional simultaneous
  regions per transaction).
- `drivers/bus/pci/src/lib.rs`, `drivers/bus/mmio/src/lib.rs` (bus driver
  side of Item 3).
- `userland/system/drvhost/src/host.rs` (the load-side surface that Item 0
  extends).

## What needs doing

### Item 0a — Resolve the `DmaPool` ↔ `VirtioHost` API shape **(blocker)**

`PLAN.md` lists three options:

- **(a) Owned `DmaSlab`** — recommended. Replace `DmaRegion<'a>` with an
  owned `DmaSlab { phys, ptr: NonNull<u8>, len, pool_id, slot }` that
  carries the disjoint-slot invariant in its fields. Add a single new pool
  accessor `DmaPool::slot_base(&self, &DmaBuffer) -> NonNull<u8>`. The
  `// SAFETY:` block on `DmaSlab::as_bytes_mut` cites the pool's slot
  bitmap (one slot ↔ one slab) as the disjointness witness. The trait
  becomes `fn alloc_dma_zeroed(&self, size: usize) -> Result<DmaSlab,
  DriverError>` — no lifetime on the return; ownership tracks the slab.
- **(b) `DmaPool::bytes_mut_n`** — rejected upstream (does not match the
  `SplitQueue::new` then loop-of-`submit` driver structure).
- **(c) One pool per virtio region** — rejected upstream (multiplies
  page-table mapping cost ×queue depth, breaks `AGENTS.md` §4 per-process
  heap rule).

Deliverables:

1. Implement option (a). The `unsafe` step in `DmaSlab::as_bytes_mut`
   carries a `// SAFETY:` block per `AGENTS.md` §2.10 and a unit test that
   exercises the disjointness invariant (allocate three slabs, hold all
   three `&mut [u8]`s live, write disjoint patterns, read them back).
2. Migrate every existing call site in `drivers/bus/virtio`,
   `drivers/storage/virtio_blk`, `drivers/network/virtio_net`, and the
   in-crate `MockHost` to the new shape. `BounceBuffer` must wrap
   `DmaSlab` (still owning the region for the transaction's lifetime).
3. `MockHost` keeps its `Box::leak` storage strategy but the leaked
   `&'static mut [u8]` is wrapped as a `DmaSlab` whose `pool_id ==
   PoolId::MOCK`. Slot tracking uses a monotonically increasing
   `Cell<usize>` inside `MockHost`; freeing in the mock is a no-op (the
   existing leak contract is unchanged).
4. Every existing test (`rustos-drv-bus-virtio` 30, `virtio-blk` 8,
   `virtio-net` 9, `kernel-mem` 84, `kernel-sec` 46) must keep passing.
   Add ≥ 3 new tests on `DmaSlab` itself (disjointness, drop-frees-pool,
   pool-id rejection across pools).
5. Update `docs/src/drivers/virtio.md`, `docs/src/architecture/memory.md`,
   and the `lib/abi` driver-trait docs. No `// HACK` / `// FIXME` /
   `#[allow]` without justification (`AGENTS.md` §15.10).

Acceptance: `cargo test --workspace --lib --exclude rustos-kernel-arch-*`
green; new `DmaSlab` tests visible in the count.

### Item 0 — Thread `DmaPool` through `userland/system/drvhost`

After Item 0a lands:

- Extend the driver-host's per-driver context with a borrowed
  `&mut DmaPool<P>` (one pool per loaded driver module, carved from the
  kernel allocator for that process).
- Implement a real `KernelVirtioHost<'a, P>` in `drivers/bus/virtio`
  (alongside `MockHost`) backed by the per-driver pool. `alloc_dma_zeroed`
  routes through `kernel_sec::alloc_dma(pool, caller_caps, size, audit)?`
  and the returned `DmaSlab::Drop` routes through `kernel_sec::free_dma`.
- The `DriverHost` trait (currently `has_capability` + `kind`) gains a
  `dma_pool(&mut self) -> &mut DmaPool<P>` accessor; the driver
  `register()` entry point receives the host context that owns it.
- Update `userland/system/drvhost` unit tests + the `drvhost_qemu`
  integration to exercise the real path.

This is a precondition for Items 2–4 working end-to-end.

### Item 2 — IRQ routing into user-space drivers

Same scope as the previous prompt:

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
  window rather than the bare identification tuple they carry today.
- The PCI and MMIO bus drivers obtain the window from the kernel via
  the DMA / future MMIO-map facility (the *kernel* allocates the
  window; the bus driver does not synthesise pointers).
- Per-bus unit tests with mock register windows; a QEMU integration
  test that walks PCI / DTB and hands a working window through to the
  virtio transport.
- Update `docs/src/drivers/bus.md` with the hand-off sequence and
  capability flow.

### Item 4 — QEMU integration tests

Once Items 0a, 0, 2, 3 are in place:

- `tests/integration/virtio_blk_pci_x86_64` — boots the kernel + driver
  host + signed `.rxe`, attaches `virtio-blk` to a backing qcow2, reads
  sector 0 (planted by `tools/qemu`), writes a known pattern to sector
  1, reads it back, verifies checksum.
- `tests/integration/virtio_blk_mmio_riscv64` — same against
  `qemu-system-riscv64 -M virt` with `virtio-blk-device`.
- `tests/integration/virtio_net_pci_x86_64` and
  `tests/integration/virtio_net_mmio_riscv64` — ARP + ICMP echo
  round-trip against `qemu user net`'s built-in DHCP/ARP/ICMP
  responder. Depends on Item 5.
- Add an unload → reload → reuse test for each driver.

### Item 5 — Userland ARP / IP / ICMP responder

The virtio-net QEMU integration tests need a small userland stack:

- New crate `userland/net/icmp/` implementing only ARP request + reply,
  IP + ICMP echo, and a minimal main loop sitting on top of the `Net`
  trait.
- Out of scope: TCP, UDP, IPv6, routing — those are Stage 6 work.

### Item 6 — Acceptance gate

After Items 0a, 0, 2–5 land:

- Run `cargo xtask ci` and paste verbatim output in the PR body.
- Run `cargo xtask test` and paste verbatim output.
- Confirm coverage ≥ 75 % on each new crate per `AGENTS.md` §7.

## Toolchain note

The pinned `nightly-2026-05-27` is required for `kernel/arch/x86_64`
(`#[unsafe(naked)]`, inline-const). On systems without `rustup` on PATH it
ships under `~/.rustup/toolchains/nightly-2026-05-27-<triple>/bin`; export
that on PATH before invoking `cargo`. The surveying session validated
`cargo test --workspace --lib --exclude rustos-kernel-arch-*` → 650
passing on that toolchain.

## Assumptions for the next session to confirm at the top of the PR body

1. Option (a) — owned `DmaSlab` — is the chosen shape. If a different
   option is justified, the next-session author records the rationale in
   `PLAN.md` and ships its tests + docs in the same commit.
2. `kernel/sec::dma::{alloc_dma, free_dma}` remain the only blessed
   capability-checked entry points; the bus + virtio drivers must not
   call `DmaPool` directly.
3. The shipped `MockHost` / `MockTransport` test seam stays in place: the
   QEMU integration tests in Item 4 are *additional*, not a replacement.
4. The `DriverHost` trait extension introduced by Item 0 is an `abi-v1`
   internal interface (not user-facing); the public driver entry point
   (`pub fn register(host: &dyn DriverHost) -> Result<DriverHandle,
   DriverError>` per `AGENTS.md` §8) is unchanged.
