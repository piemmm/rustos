# Next session — Stage 4.D items 2–6 (virtio end-to-end on real hardware)

## Where we are

Item 1 of the previous next-session prompt landed in this session
under the agreement "land Item 1 only, defer 2–6". The deliverables
are summarised in the Stage 4 status block of `PLAN.md`; the headline
items:

- `lib/abi`: `CapabilityId::MEM_DMA = 10`, frozen ID test, added to
  `kernel/sec::manifest::is_known_capability` (`abi-v1` only — do
  not renumber).
- `kernel/mem`: new `dma` module (`DmaPool<P>`, `DmaBuffer`,
  `DmaError`). Per-process virtual window over `AddressSpace<P>`,
  contiguous frames from `FrameAllocator::alloc_order`, guard slots
  (unmapped on hardware, `0xCC` on host), zero-on-free via the
  audited `zeroize` crate, `Result<_, _>` for every failure path.
  New `host-tests` Cargo feature exposes `HostPageTable` to
  downstream test crates without leaking it into production builds.
- `kernel/sec`: companion `dma` module with `alloc_dma` / `free_dma`
  that gate `CapabilityId::MEM_DMA` and emit
  `AuditEvent::DmaAllocated = 1030` / `DmaAllocDenied = 1031` on
  every grant or refusal. `DmaGateError::as_errno()` lands the
  `abi-v1` mapping for the future syscall wrapper.
- Tests: `cargo test -p rustos-kernel-mem --lib` → 84 passing,
  `cargo test -p rustos-kernel-sec --lib` → 46 passing. No
  regressions in `rustos-abi`, `rustos-caps`, `rustos-drv-bus-virtio`,
  `rustos-drv-storage-virtio-blk`, `rustos-drv-network-virtio-net`,
  `rustos-drv-bus-pci`, `rustos-drv-bus-mmio`, `rustos-drvhost`.
- Docs: new "DMA buffers" section in
  `docs/src/architecture/memory.md`; the audit catalogue in
  `docs/src/architecture/security.md` gained the `1030 / 1031`
  rows.

What still needs doing is everything *Item 2* onward from the prior
prompt, plus a small first-time-only follow-up triggered by the new
`DmaPool` (see Item 0 below).

## Reading list

- `AGENTS.md` (binding).
- `PLAN.md` Stage 4 status block, including the new Item-1
  paragraph.
- This file.
- `kernel/mem/src/dma.rs`, `kernel/sec/src/dma.rs` (the seam Items
  2 and 3 plug into).
- `drivers/bus/virtio/{src/host.rs, src/transport.rs}` (the
  consumer side of the kernel API).
- `drivers/bus/pci/src/lib.rs`, `drivers/bus/mmio/src/lib.rs`
  (bus driver side of Item 3).

## What needs doing

### Item 0 — Thread `DmaPool` through `userland/system/drvhost`

The Item-1 work built the kernel side of the DMA facility but did
**not** wire it through the driver host yet. The current
`MockHost::alloc_dma_zeroed` is still the test seam used by
`virtio_blk` / `virtio_net`. This session must:

- Extend the driver-host's per-driver context with a borrowed
  `&mut DmaPool<P>`. The host creates one pool per loaded driver
  module out of the kernel allocator carved for that process.
- Implement a real `VirtioHost` in `drivers/bus/virtio` (alongside
  `MockHost`) backed by the new pool — `alloc_dma_zeroed` calls
  `kernel_sec::alloc_dma(pool, caller_caps, size, audit)?`,
  releases through `kernel_sec::free_dma`.
- Update `userland/system/drvhost` tests + the `drvhost_qemu`
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

Once Items 0–3 are in place:

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

After Items 0–5 land:

- Run `cargo xtask ci` and paste verbatim output in the PR body.
- Run `cargo xtask test` and paste verbatim output.
- Confirm coverage ≥ 75 % on each new crate per `AGENTS.md` §7.

## Toolchain note for the next session

`cargo xtask test` could not be run end-to-end in this session
because `kernel/arch/x86_64` requires the `nightly-2026-05-27`
toolchain pinned in `rust-toolchain.toml` (`#[unsafe(naked)]` and
inline-const) and the host had only stable rustc 1.75. The next
session must run on an environment with the pinned nightly so the
acceptance gate above is meaningful — the Item-1 changes themselves
were verified per-crate (`cargo test -p rustos-kernel-mem -p
rustos-kernel-sec --lib` → 130 passing).

## Assumptions for the next session to confirm at the top of the PR body

1. The `DmaPool` / `DmaBuffer` / `DmaError` surface in
   `kernel/mem::dma` is the right seam for both the driver host
   (Item 0) and the bus drivers (Item 3). If a different shape is
   needed (e.g. iommu translation), propose it in `PLAN.md` rather
   than mutating in place.
2. `kernel/sec::dma::{alloc_dma, free_dma}` are the only blessed
   capability-checked entry points; the bus + virtio drivers must
   not call `DmaPool` directly.
3. The shipped `MockHost` / `MockTransport` test seam stays in
   place: the QEMU integration tests in Item 4 are *additional*,
   not a replacement.
