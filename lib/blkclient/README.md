# tairix-blkclient

The TAIRiX block-service **client** (`plans/FIX-IO.md` IO6): the one
`no_std` `tairix_abi::driver::block::Block` implementation over the
fixed-frame `tairix_abi::blkio` request/reply pair, plus the production
async transport (`RtBlkCall`) its consumers issue that protocol over.

A user-space block driver (the first is the USB mass-storage class driver)
exposes each logical unit it brings up as a **block-service call endpoint**
plus a shared-memory data window, both forwarded as grants on the
storage-class hardware-tree node it emits. Two consumers drive a served
device through this crate's `RemoteBlock`:

- the volume-manager policy driver (`drivers/storage/volmgr`), which only
  ever inspects a device's layout (partition table, filesystem signatures)
  and commits nothing to it, and
- the RAID array composer, which both reads a member's superblock and
  durably writes to the array it assembles.

A copy of this client per consumer would let the wire discipline, the
geometry validation, and the bounded-reissue policy silently drift between
them, so it lives here once and both consumers link it.

## What it provides

- `RemoteBlock::connect_read_only(call, window)` / `connect_read_write(call,
  window)` — connect and validate a device's geometry, opening the client
  under an explicit, named access stance. A read-only client never lets a
  write or a flush reach the wire, however the device answers; a
  read/write client allows both, still subject to the device's own
  write-protect flag. Which stance a caller holds is therefore visible in
  the type of constructor it called, not an accident of which methods it
  happens not to invoke.
- `BlkCall` — the transport seam `RemoteBlock` is generic over, so the
  client is host-testable without a kernel: a host test double fills the
  shared window directly, playing the serving driver.
- `RtBlkCall::new(endpoint, waitset)` — the production transport:
  `call_post`/`call_reap` on the granted endpoint with the caller's own
  per-request deadline, parking on the **caller's** `CallReply` wait-set
  rather than a busy poll, so a wedged device fails a transfer closed at its
  deadline instead of parking the caller forever.

## Design

- **The caller owns the wait-set.** A transport parks on a set it is handed,
  never one it mints for itself. The kernel reclaims a wait-set only when its
  owning process exits, so a self-minting transport would strand one per
  instance — harmless in a run-to-completion program like the volume manager,
  but an unbounded kernel-memory leak in a long-lived one that opens a
  transport per device or per retry, as the RAID composer does on every
  assembly attempt. Handing the set in makes it part of the process's own
  one-time setup whatever its shape, and lets one set carry every member of a
  multi-device consumer. Readiness is level-triggered, so sharing a set costs
  nothing but an occasional re-check and consumes no wake from its owner.
- Everything a served device reports is untrusted: the geometry is
  validated at connect time before any consumer sees it, every reply frame
  is decoded fail-closed, and a transfer never reads more bytes out of the
  shared window than the request named.
- The bounded-reissue policy (`IoBudget::should_reissue`) is the one
  definition both a read and a write share, derived from the device's own
  declared class rather than an assumed envelope, so a removable unit
  riding out a bus reset and a paravirtual device that has simply wedged
  are each given their own class's patience.
- A device need not declare a class this ABI recognises, so the class on a
  completion is an `Option`: an unrecognised class word decodes to *unknown*
  and stays unknown instead of being renamed to a class nobody reported.
  Patience still needs a concrete envelope, and `BlkDeviceClass::served_as`
  is the single policy that picks one — an unknown device is served the
  bounded unclassified envelope and no extra patience. Hence `device_class`
  reports what the device is *served as*, while `declared_class` reports
  what it actually said.
- `RtBlkCall` builds cleanly on the host (its syscall trap fails closed
  with a sentinel there rather than requiring a kernel), so this crate
  needs no target-conditional gate to stay host-testable end to end.

## Stability

Tier: `experimental`.
