# tairix-netchan

TAIRiX NIC device-channel driver side: the `netchan-v1` server every network
driver *process* runs (`plans/NETWORK.md` N4d, N17). Stability tier:
**experimental** — it tracks the unfrozen `abi-v1` `netchan-v1` contract.

The network stack runs in its own address space and owns the shared frame
region; a NIC driver owns its device (MMIO, DMA, interrupt) and serves a call
endpoint. This crate is everything a driver process must do *around* an
opened `Net` device, written once rather than copied per device, so a driver
is device bring-up plus one `serve` call.

## The two halves

* `server` — `NetChannelServer`, the pure, host-testable request handler: the
  detached/attached state machine, the geometry validation, the ring service,
  the group filter, and the receive pre-filter it installs. It performs no
  I/O, so the whole control plane is exercised on the host against a mock
  `Net`.
* `serve` — the freestanding process loop (bare-metal targets only): it
  claims a reserved device-channel endpoint bound restricted-sender, emits
  the `netchan` hardware-tree node the device manager binds to the stack, and
  parks on a wait set over `{call endpoint, device interrupt}`.

## The interrupt path

Two costs were removed here, and both are worth stating because the naive
shape — acknowledge, notify, re-park — is pathological on real hardware.

**Masking.** A DMA engine's completion status latches a *level* condition
("completed descriptors are waiting"). Acknowledging clears the latch, but
with frames still undrained the condition re-latches at once, and the kernel
re-arms the interrupt line every time the process parks. A driver that only
acknowledged and notified therefore spun interrupt → acknowledge → notify →
park at the speed of a context switch until the stack caught up — measurable
as a permanently busy core on an otherwise idle machine. So the loop masks
the device's data-path completion sources on entry and unmasks them only once
the device is empty *and* the shared receive ring has room. A burst then
costs one interrupt rather than one per frame.

`DrainStep` is that decision as a pure value, so the policy is host-tested
without hardware while the loop supplies the mask-register write. Link and
configuration-change sources are never masked, or a cable pulled mid-flood
would go unnoticed; the completion sources are masked whenever the channel is
detached, so a device left running cannot storm a driver with nowhere to put
frames.

**Harvesting.** The frame region is already mapped here, so making the stack
ask for frames with a blocking call cost two extra process switches per batch
for nothing. The interrupt fills the ring and rings the doorbell once,
carrying the live link and a back-pressure flag; the stack reads the ring in
its own time and calls back only when it has transmit work or a masked source
to release. The ring's atomic counters are what make that safe.

## Fail closed

Every reply is a fully-encoded `netchan-v1` frame carrying a typed `Errno`:
a service before attach, a region that does not match the agreed geometry or
is not aligned for the ring counters, a geometry too small for the device's
frames, or any device fault. Never a panic, never a partially-applied action.
Set-up refusals in `serve` return a reserved `exit` code, so a driver that
cannot serve ends with a diagnosable reason rather than degrading into a busy
re-poll.
