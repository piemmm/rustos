# `tairix-netchan`

`lib/netchan` is the **driver side of the `netchan-v1` NIC device-channel
contract** (`plans/NETWORK.md` §2.3, N4c/N4d/N14a): everything a link-layer
driver process must do around an opened device to serve the network stack,
written once so every NIC driver shares one control plane.

## Why it exists

The stack (`userland/net/netstack`) and a NIC driver run as separate
processes. The stack owns the shared frame-ring region and is the channel's
*client*; the driver owns the device (MMIO/DMA/IRQ) and is its *server*. The
wire codecs for that contract live in `lib/abi::driver::net_channel`, but the
*server behaviour* is not a wire type — it is attach state, geometry
validation, and a wait-set loop. That behaviour originally lived in
`lib/virtio_net`, which meant a second NIC driver would have had to link the
virtio-net device engine to reach it. It lives here instead, over the
`tairix_abi::driver::net::Net` trait, so a driver for any silicon composes it.

Today both `drivers/network/virtio_net_driver` and
`drivers/network/genet` are that composition, and each is reduced to
device bring-up plus one `serve` call.

## Two layers

`NetChannelServer<N: Net>` is the pure, host-testable per-request handler. It
performs no I/O: the caller receives the request, maps the granted region, and
sends the reply this server produces, so the whole control plane is exercised
on the host against a mock device.

- A fresh server is **detached**: it answers `Facts` (so the stack can size
  the ring geometry from the device's MTU) and refuses `Service` with
  `NotConnected`.
- `Attach` validates the offered geometry against the device — both
  directions must carry at least one device frame, and a segmenting device's
  transmit ring must carry a super-frame — and moves to **attached**. A
  refused attach leaves the server unchanged; it never half-binds.
- `Service` binds a `FrameRings` view over the caller-mapped region and
  drives exactly one `Net::service` doorbell.
- `Detach` returns to detached; the device stays live for a later re-attach.

`Drain` is its interrupt-path counterpart, and pure for the same reason: it
folds in one `ServiceReport` at a time and answers what the loop must do next
(service again, unmask and look once more, re-mask and resume, stop) plus,
at the end, what the stack must be told. Getting that wrong is not a subtle
bug — unmasking into a still-asserted level condition spins the driver, and
stopping while masked without saying so wedges the interface — so the whole
policy is decided where a host test can drive it, and the loop supplies only
the mask-register writes and the notify.

`Masked` is the one name for "the completion sources are still down": the
receive ring filled (`BackPressure`), the round budget ran out
(`BudgetSpent`), or a service faulted (`Fault`). All three mean the same
thing to the stack — only its next `Service` can release or diagnose them —
so all three set the notify's back-pressure flag.

`serve(net, irq_handle)` is the freestanding process loop, compiled only for
the bare-metal targets a driver binary is built for. It:

1. claims the first free id in the reserved device-channel endpoint block,
   bound **restricted-sender requiring `CAP_NET_RAW`** — so the kernel refuses
   every caller but the stack at dispatch, on top of the
   `CAP_IPC_BIND_PRIVILEGED` gate the reserved-id bind already demands;
2. publishes a hardware-tree node carrying that endpoint as a grant request
   (`NETCHAN_NODE_COMPATIBLE`), which the device manager observes and hands to
   the stack over its capability-gated admin surface. The kernel re-parents
   the node under the driver's *own* matched node, which is how `devmgr`
   recovers the NIC's stable bus location from the published channel;
3. parks on a wait set over `{call endpoint, device interrupt}` for the life
   of the driver — never busy-polls (`AGENTS.md` §2.23). A call wake serves
   one request. An interrupt wake **masks** the device's completion sources
   first (acknowledging alone would not stop a re-fire: the latch sits over a
   level condition that is still true while frames are undrained), then
   acknowledges, then harvests the rings into the shared region itself under
   `Drain`, and wakes the stack with a single notify. The notify carries what
   the driver already knows — the live link, whether a source is still masked,
   and the device's cumulative pre-filter count — so a pure receive costs the
   stack no `Service` call at all. That last field is not a convenience: a
   receive-only interface rings no doorbell, so without it the operator's
   `stats:net/<iface>/rx.filtered` would sit frozen at whatever the last
   transmit happened to observe.

## Fail closed

Every reply is a fully-encoded `netchan-v1` frame carrying a typed `Errno`: a
service call before attach, a region too small for the agreed geometry, or a
device fault is never a panic and never a partially-applied action. Every
set-up refusal in `serve` returns a reserved code from the `exit` module
(80–83), so a driver that cannot serve its device ends with a diagnosable
reason rather than degrading into a busy re-poll.

Those codes are the diagnosis a supervisor reads off a driver that gave up,
so they are defined here rather than per driver: two drivers reporting the
same failure report the same number.

## Test surface

`cargo test -p tairix-netchan` drives `NetChannelServer` against an
in-process loopback `Net`: the facts reply and its device-fault path, the
detached/attached transitions, a frame round-tripping through one service
doorbell, a geometry too small for the device, a wrong-length region, and
detach. `Drain` is driven directly over synthetic reports: the re-arm
look-once-more, a frame landing in that window, a full ring, a fault, a
link change with no frame moved, the pre-filter count reaching the notify,
and an exhausted round budget stopping masked *and* asking for release. The process loop is exercised live by the two-process QEMU verticals
(`netstack_autoload_qemu_*`, `netstack_dhcp_qemu_*`,
`netstack_dhcp6_qemu_*`), which drive the real stack against the real server
across a process boundary.
