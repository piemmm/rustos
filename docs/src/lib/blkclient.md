# `tairix-blkclient`

`lib/blkclient` is the **block-service client**: the one `no_std`
`tairix_abi::driver::block::Block` implementation over the fixed-frame
`tairix_abi::blkio` request/reply pair, plus the production async transport
(`RtBlkCall`) its consumers issue that protocol over (`plans/FIX-IO.md`
IO6).

## Why it exists

A user-space block driver (the first is the USB mass-storage class driver,
`drivers/storage/usb_msd`) exposes each logical unit it brings up as a
**block-service call endpoint** plus a shared-memory data window, both
forwarded as grants on the storage-class hardware-tree node it emits. Two
consumers inherit those grants and drive the device through `RemoteBlock`:

- the volume-manager policy driver (`drivers/storage/volmgr`), which probes
  a device's partition table and filesystem signatures and never commits
  anything to it, and
- the RAID array composer, which both reads a member's superblock and
  durably writes to the array it assembles.

The client used to live inside `drivers/storage/volmgr` as a read-only
implementation. That put it in a `drivers/*` crate the RAID driver could not
depend on without a `drivers/*`→`drivers/*` layering edge, and it had no
write path at all. Moving it into `lib/*` and completing its write path
gives both consumers the identical wire discipline, geometry validation,
and bounded-reissue policy from one definition, rather than risking two
copies drifting apart.

## Read-only vs. read/write: an explicit stance, not an accident

`RemoteBlock` is opened through one of two named constructors:

- `connect_read_only(call, window)` — `write_blocks` always refuses and
  `flush` is a truthful no-op, regardless of what the device reports. This
  is the volume-manager probe's stance: it inspects a layout and commits
  nothing, so it is given no authority to change one.
- `connect_read_write(call, window)` — `write_blocks` and `flush` reach the
  wire, still refused when the device itself reports its write-protect
  flag.

Both refusal reasons — the client's own opened stance, and the device's own
declared flag — are checked before any byte moves, so a caller cannot
accidentally widen its own authority by forgetting which methods it should
avoid calling, and a client cannot be tricked into writing to a
write-protected device by either side alone.

## The transport seam and the production transport

`RemoteBlock` is generic over `BlkCall`, one synchronous request/reply
exchange with the serving block driver. The production implementation,
`RtBlkCall`, issues `call_post`/`call_reap` on the granted endpoint with
the caller's own per-request deadline (derived from the device's declared
`BlkDeviceClass`, never a value the transport chooses for itself), parking
on a `CallReply` wait-set rather than a busy poll when a reply is not yet
ready. A wedged device therefore fails a transfer closed at its deadline
instead of parking the caller forever.

A served device need not declare a class the ABI recognises, so what the
completion carries is an `Option<BlkDeviceClass>`: a class word this version
does not define decodes to *unknown* and stays unknown rather than being
rewritten to a class nobody reported. Sizing a deadline still needs a
concrete envelope, and `BlkDeviceClass::served_as` is the one policy that
supplies it — an unknown device is served the same bounded unclassified
envelope a paravirtual device gets, and no extra patience. `device_class`
therefore reports the class the device is *served as*; `declared_class` is
what it actually said.

That wait-set is the **caller's**, supplied to `RtBlkCall::new(endpoint,
waitset)`. A wait-set is reclaimed only when its owning process exits, so a
transport that minted one for itself would strand a kernel object per
instance. That is invisible in a run-to-completion consumer like the volume
manager, which opens exactly one transport and then exits — but the RAID
composer is long-lived and reconnects a member device on *every* assembly
attempt, so a self-minting transport would leak a wait-set per attempt and an
array that can never be assembled would drive that leak on its backoff timer
indefinitely. Taking the set from the caller makes it part of that process's
own one-time setup whatever its shape, and lets a single set carry every
member of a multi-device consumer. The transport adds its endpoint's
`CallReply` member on first use and treats an already-present membership as
success, so several transports may legitimately share one set; readiness is
level-triggered, so a wake caused by an unrelated member simply re-checks and
parks again, and no wake is consumed away from the set's owner.

`RtBlkCall` builds on `tairix-rt`'s syscall wrappers, which themselves
build cleanly on the host (the underlying syscall trap fails closed with a
sentinel there rather than requiring a live kernel), so this crate needs no
target-conditional gate: a host test double plays the serving driver by
filling the shared window directly, and every geometry-validation,
chunking, and bounded-reissue path is exercised without a kernel.

## What the served backing can promise

A device served over this seam may itself be a composition — a RAID array over
several disks — and a consumer that layers on top of this client (a further
composition, a filesystem) must know when the thing underneath is short of
redundancy. Each completion's health status is therefore reflected onto an
availability reading through the one shared
`MountAvailability::from_block_status` mapping, and
[`Block::backing_availability`](../drivers/block.md) reports it. There is no
second state machine: the serving driver owns the sticky one and its recovery
grace window, and this client mirrors its per-request verdict. A per-request
verdict that says nothing about the *volume* — a bad sector — leaves the reading
alone, and until the device has answered anything there is nothing to stand
background work down for.

## Fail-closed by construction

Everything a served device reports is untrusted: the geometry is validated
at connect time before any consumer sees it (block size a power of two in
512..=4096, a non-zero block count that does not overflow, a window large
enough for one block), every reply frame is decoded fail-closed, and a
transfer never reads more bytes out of the shared window than the request
named. A write's shape and extent are validated against the cached
geometry before any byte reaches the wire, exactly like a read.

## Stability

Tier: `experimental` (see the crate `README.md`).
