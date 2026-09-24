# DMA-engine drivers

A DMA controller moves data between memory and other devices' FIFOs, so that
an audio, SPI, SD or UART driver can stream without the CPU copying each word.
`HwDeviceClass::Dma` names the class and its drivers live under
`drivers/dma/<leaf>/`. The staged design is `plans/SOUND.md` §The DMA-engine
seam (SND5); this page is the class view of what exists.

## Who may write a control block

A controller fetches control blocks from memory, and a control block holds
bus addresses. With no IOMMU between the controller and RAM, whoever writes
one can read and write all of memory. The controller's driver is therefore
the only process that maps the controller's registers or writes its control
blocks, and a consumer driver never supplies an address. It quotes claims the
kernel attests:

- its **request line**, a `DmaRequest` grant discovery built from its node's
  `dmas` entry, which the controller checks the calling process holds;
- its **FIFO**, a CPU-physical address inside one of its own register
  windows, which the controller translates through its own DMA window.

The buffer comes back as a shared-memory grant the controller carved under
its own addressing constraint, so every block's memory side lies inside that
channel's own buffer by construction.

## Discovery

The shared device-tree walk reads the generic DMA binding for every FDT port.

- **A controller** is any node with `#dma-cells`. It carries a
  `DmaController` duty naming its endpoint — one per controller node, from
  the reserved `DMA_CONTROLLER_ENDPOINTS` block indexed by node id — and the
  channels the tree leaves to this system (`dma-channel-mask`, numbered from
  the node's own first channel, or a port's vendor spelling converted to
  that numbering). The duty records whether the tree stated a mask at all.
- **Its windows**: one `Dma` resource per entry of its parent bus's
  `dma-ranges`, translated to CPU addresses, carrying the bus address it
  starts at, and flagged `DMA_TRANSLATED` so a window starting at bus `0` is
  never read as an untranslated limit. A controller with no bus between it and the root, or on a bus
  whose property is empty, reaches memory untranslated and gets one
  unconstrained window; a bus with no property maps nothing.
- **A consumer's request lines**: each `dmas` entry becomes a `DmaRequest`
  naming its controller's endpoint, the specifier in the controller's own
  binding (up to two cells — a wider entry is dropped, never truncated), the
  entry's position, and its `dma-names` string where that fits eight bytes.
  A phandle resolves to the id the walk gives its controller by replaying the
  walk's emission rule, so a consumer met before its controller still names
  the right endpoint.

A `DmaRequest` grant covers *calling* its controller's endpoint and never
binding it, so no consumer can serve the rendezvous every other consumer of
that controller calls. Both record kinds decode only from their canonical
encoding, so a record a controller receives quoted re-encodes to exactly the
bytes the kernel holds as the caller's grant.

## `dmaengine-v1`

`tairix_abi::driver::dmaengine` is the controller endpoint's protocol. Every
frame is exact-length and every decode total.

| Operation | Carries | Answers |
|---|---|---|
| `Open` | the caller's request line | the lowest free channel the mask allows, at most one per request |
| `Prepare` | channel, FIFO, direction, period bytes, periods | the grant for the caller's mapping of the buffer |
| `Start` | channel | — |
| `Stop` | channel | — (abort, then channel reset) |
| `Position` | channel | the live memory-side offset |
| `Close` | channel | — |
| `Wait` | channel, a byte position | a report at the first period boundary past it |

`Wait` is a posted call. Its report carries the monotone byte position and
the controller's monotonic clock when it serviced the event, and says whether
the wait ended at a boundary, because the channel stopped, or because it
faulted with the controller's own error bits.

A transfer is cyclic: `Prepare` builds one interrupting block per period
over a buffer holding the periods end to end, looping until stopped. The
seam has no pause, because a paused request-paced transfer starves its
peripheral, and no memory-to-memory or one-shot scatter-gather transfer.

## Kernel mechanisms

- **The endpoint is the duty holder's alone.** Binding an id in
  `DMA_CONTROLLER_ENDPOINTS` requires holding the `DmaController` duty that
  names it — not merely the privileged bind — because every consumer holds a
  request line naming the same id, and one of them serving it would answer
  all the others.
- **`call_peer_holds`** answers whether the caller being served holds a grant
  covering a quoted record, so the controller checks a request line, or a
  FIFO's register window, against the kernel's grants rather than the
  client's word.
- **`shm_create_dma`** carves a channel's buffer below the controller's own
  `Dma` window, mapped coherent in every process that maps it, and returns
  the bus address the controller programs; **`shm_grant_peer`** mints that
  buffer to the client whose call is being served.
- **The buffer outlives a crash safely.** The controller keeps its own
  mapping while a channel may master the buffer — its unmap is its word that
  the device is done — and should it end still mapping it, the buffer joins
  its node's quarantine when the client lets go, freed only after the next
  instance declares the controller reset.

## Status

The record kinds, the discovery, the endpoint block, the protocol and the
kernel mechanisms exist. The class trait and the first controller driver,
`drivers/dma/bcm2835`, are `plans/SOUND.md` SND5c.
