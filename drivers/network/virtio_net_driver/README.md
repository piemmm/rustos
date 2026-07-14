# `rustos-drv-network-virtio-net-driver`

The user-space **virtio-net driver process** — the `Run` entry point of the
signed `/System/Drivers/network/virtio_net/` bundle, autoloaded by `devmgr`
when a virtio-net device is discovered (`AGENTS.md` §18, `plans/NETWORK.md`
N4d).

Stability tier: **experimental**.

## What it is

RustOS runs its network drivers in user space (`AGENTS.md` §4,
microkernel-leaning). This process owns one virtio-net NIC — its MMIO register
window, its DMA, and its interrupt line, all delivered as the capability
grants its matched hardware-tree node requested — and serves the `netchan-v1`
device-channel contract (`rustos_abi::driver::net_channel`) to the network
stack (`userland/net/netstack`).

The two run in **separate address spaces** and never link each other: the
driver is the *server* of a device-channel call endpoint it claims from the
reserved `NET_CHANNEL_ENDPOINT_*` block, and the stack is the *client* that
owns the shared frame-ring region. Any NIC driver serves any stack build
(`AGENTS.md` §17.4 — a process crate depends on `lib/*` only, never on another
`drivers/*` crate).

## How it works

1. Builds the rt-backed `RtDriverHost` (`lib/drvrt`) from its kernel-issued
   grants, maps the single register window the node named, and brings the
   device up over the bus-agnostic virtio-MMIO transport (`lib/virtio`) and
   the virtio-net device engine (`lib/virtio_net`).
2. Binds the granted device interrupt line (the audited readiness witness) and
   claims the first free reserved device-channel endpoint, binding it
   **restricted-sender requiring `CAP_NET_RAW`** so only the stack can post
   (defence in depth atop the reserved-bind `CAP_IPC_BIND_PRIVILEGED` gate).
3. Publishes a `netchan` hardware-tree node carrying the claimed endpoint, so
   `devmgr` observes it and hands the endpoint to the stack over the
   capability-gated admin surface.
4. Parks on a wait set over the call endpoint and the device interrupt, never
   busy-polling (`AGENTS.md` §2.23): a doorbell drives the pure
   `NetChannelServer` (`Facts`/`Attach`/`Service`/`Detach`); an interrupt is
   acknowledged (so the line never storms) and, when a region is attached,
   wakes the stack with a single receive-frames notify.

## Supported hardware

virtio-net over virtio-MMIO (the transport QEMU's `-M virt` boards present)
and, transitively through `lib/virtio`, virtio-PCI. It names no board, bus, or
transport detail: MMIO base, DMA constraint, and IRQ line are all discovered
values threaded from the matched node (`AGENTS.md` §2.20).

## Required capabilities

`CAP_MMIO_MAP`, `CAP_MEM_DMA`, `CAP_IRQ_BIND` (the device resources),
`CAP_IPC_ENDPOINT` + `CAP_IPC_BIND_PRIVILEGED` (claiming and binding the
reserved device-channel endpoint), `CAP_HW_EMIT` (publishing the `netchan`
node), and `CAP_LOG_EMIT`. The effective set is the intersection of these
with the launching identity's grants; the kernel re-checks every trap.

## Limitations

Runtime load/unload follows the standard driver lifecycle (`AGENTS.md` §8):
one instance is spawned per discovered virtio-net node, and a device fault or
a lost endpoint exits the process fail-loud for a clean reload. Offloads are
not yet negotiated (that is `plans/NETWORK.md` N7).

## Tests

The driver's device logic and its per-request handling are the
`lib/virtio_net` engine and `NetChannelServer`, host-tested there. The
freestanding `Run` body is exercised by the two-process netstack QEMU
verticals (`tests/integration/netstack_*`).
