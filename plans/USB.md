# USB.md — Modular USB stack and device hot-removal

This is the staged build plan for TAIRiX's **modular USB stack**: a clean
split between the host-controller driver, the per-device class drivers, and
the device manager's hot-removal reaction. It is the continuation of the
Design-D device-manager work in `plans/PI.md` (the user-space autoload chain
and the `hw_emit_node` / `hw_remove_node` syscalls) and the layering the §17
modularity contracts require.

`AGENTS.md` is binding — read it and `PLAN.md` first. Every rule in this file
is binding too.

**Note:** `abi-v1` is *not* frozen, despite what `AGENTS.md` / `PLAN.md` say —
the standing task direction supersedes that language. Changing a `lib/abi`
type today is allowed; it requires regenerating the C header
(`cargo xtask c-header --write`), which the drift guard enforces.

---

## 0. The problem this plan fixes

The metal-proven keyboard chain (`plans/PI.md` B5/D5) folded two distinct
responsibilities into one process. Today `drivers/input/usb_kbd` binds the
`usb,xhci` controller node **directly** and performs the *whole* job:

1. maps the controller's register BAR, owns its DMA rings and root-hub ports,
   brings the xHCI controller up, and enumerates the attached device; **and**
2. decodes HID boot-keyboard reports and injects keystrokes.

Step 1 is a **host-controller driver** (HCD) responsibility — it is per
*controller*, identical for every device behind that controller. Step 2 is a
**class driver** responsibility — it is per *device*, and a keyboard, a mouse,
and a mass-storage device each need a different one. Fusing them into one
process means:

- **No modularity (§17).** A second USB device behind the same controller has
  nowhere to bind: the single process already owns the controller. Supporting
  a USB mouse alongside the keyboard would mean a second process re-opening
  the same controller registers — two masters on one xHCI, which has no
  defined ownership model and is unsafe.
- **Hot-removal has no clean shape.** When the keyboard is unplugged the
  controller node is still present (the controller did not vanish), so there is
  no per-device node for a watcher to remove, and removing the controller node
  would tear down the controller driver itself.
- **A bus↔class hardwiring risk.** The keyboard driver knows it sits behind an
  xHCI controller. A class driver must not know *which* controller (or bus —
  the same device class can appear behind a different host controller, or on a
  non-PCIe transport), and a controller driver must not know *which* class
  drivers exist. TAIRiX may carry several controller drivers and several class
  drivers; none may be wired to a specific sibling.

`drivers/bus/usb/xhci` already exists as the intended HCD crate, but it is
currently only a vestigial kernel-linked `register` / `BIND_KEYS` stub — the
live path bypasses it. This plan makes it the real HCD and reduces `usb_kbd`
to a pure HID class driver.

---

## 1. Target architecture (binding)

Three independent layers, each reached only through discovery and the public
ABI — never by naming a sibling crate (§17.4, §2.20):

```
            hardware tree (lib/abi hwtree)            URB transport IPC (lib/abi)
                  │  emits / removes nodes                  │  submit / complete
  ┌───────────────┴───────────────┐         ┌──────────────┴───────────────┐
  │  Bus driver(s)                 │         │  USB host-controller driver   │
  │  drivers/bus/pcie_brcm, …      │  emits  │  (HCD) drivers/bus/usb/xhci   │
  │  trains the link, enumerates   │ ──────▶ │  owns ONE controller:         │
  │  the controller function,      │ usb,xhci│   registers, DMA rings,       │
  │  emits the controller node     │  node   │   root-hub ports, enumeration │
  │  with its BAR + DMA + IRQ      │         │  emits ONE node per attached  │
  │  grants. Knows nothing of USB. │         │  USB interface; serves URB    │
  └────────────────────────────────┘         │  IPC; watches PORTSC.         │
                                              └──────────────┬───────────────┘
                                                             │ emits / removes
                                                             │ per-interface node
                                              ┌──────────────┴───────────────┐
                                              │  USB class driver(s)          │
                                              │  drivers/input/usb_kbd (HID), │
                                              │  a future usb_mouse, …        │
                                              │  binds an emitted interface   │
                                              │  node, submits/reaps URBs over│
                                              │  the transport IPC. Touches   │
                                              │  NO controller register, knows│
                                              │  no bus/controller identity.  │
                                              └────────────────────────────────┘
```

### 1.1 The host-controller driver (HCD)

- Lives in `drivers/bus/usb/xhci` as a user-space `Run` binary (it is a driver,
  not a `lib/*` crate; the bus-agnostic xHCI *protocol* engine stays in
  `lib/usb`, §2.22). It binds the `usb,xhci` controller node by the xHCI PCI
  class key it already declares in `BIND_KEYS`.
- It is the **sole owner** of one controller: its register BAR, its DMA command
  / event / transfer rings, and its root-hub ports. No other process maps that
  BAR. This is the ownership model that makes "two processes touching one
  xHCI" a non-problem: only the HCD ever does.
- On attach it enumerates the device (address, descriptors, configuration) and
  **emits one hardware-tree node per USB interface** through `hw_emit_node`,
  carrying USB match keys (`vid:pid:class:subclass:protocol`) so the device
  manager can match a class driver — and carrying **no** controller register
  grant. The interface node's only "resource" is the right to talk to the HCD's
  URB transport endpoint for that interface (see §1.3).
- It **watches the root-hub PORTSC** event-driven (xHCI Port Status Change
  events on its event ring; never a busy-poll, §2.23). On a disconnect it calls
  `hw_remove_node` on the affected interface node(s); the device manager reacts
  by unloading the bound class driver (§1.4). On a fresh connect it enumerates
  and emits, and the device manager autoloads the matching class driver — so
  re-plug works with no reboot (§18.4).
- It knows nothing of PCIe, the VL805, or any board: it received its register
  window and DMA constraint as grants on its matched node, exactly as today
  (`usb_kbd`'s current platform-neutral bring-up moves here unchanged).
- **It is a single asynchronous event loop, never a per-URB blocking server
  (§2.23).** The HCD must service two independent event streams at once —
  incoming URB-submit IPC calls on the per-interface endpoints it serves, and
  its controller's completion/PORTSC interrupts on the event ring — for
  arbitrarily many interfaces (the USB-mouse-alongside-keyboard case, §0).
  Blocking inside one interface's URB handler would stall every other
  interface, so the loop **multiplexes**: it parks on a kernel **wait-set**
  (`U3a3`) holding its URB-submit endpoints *and* its controller IRQ line,
  wakes on whichever is ready, and either accepts a new URB (queues it on the
  ring, leaving the caller's URB call outstanding) or drains the event ring on
  an interrupt and **replies to the now-complete URB calls** — completions are
  delivered asynchronously, not synchronously per submit. One CPU-free park
  covers all interfaces; an idle controller burns nothing.

### 1.2 The class driver

- `drivers/input/usb_kbd` becomes a pure HID boot-keyboard **class driver**. It
  binds an emitted USB-interface node matched by the HID boot-keyboard class
  key — never the controller node.
- It owns **no** controller register and carves **no** controller DMA. It
  submits interrupt-IN transfers (URBs) for its interface and reaps completions
  over the URB transport IPC (§1.3), decoding each HID boot report through the
  arch-neutral `tairix_hid` composition and injecting keystrokes through the
  existing `key_inject` syscall.
- It knows neither the controller type nor the bus: the same binary would work
  unchanged behind a different host controller, because it speaks only the
  bus-agnostic URB transport ABI.

### 1.3 The URB transport IPC (the seam)

- A new versioned, hashed, capability-checked ABI in `lib/abi/src/usb_urb.rs`
  (held to the syscall-table discipline, §9): a **URB** (USB request block) is a
  typed request — endpoint address, transfer type (control/interrupt/bulk),
  direction, a shared-memory data buffer handle, and a length — and a typed
  completion — status, bytes transferred. It carries no controller detail.
- The HCD serves a URB transport **call endpoint** per interface it emits
  (`lib/usb` gains the controller-side "transport server" and the class-side
  "transport client" over this ABI; the multi-device enumeration
  engine is owned by the HCD, not the class driver). The interface node the
  HCD emits names the endpoint id the class driver connects to as its sole
  "resource" — so the capability to submit URBs for an interface is minted
  kernel-side from the matched node, never ambient (§4, §5.4).
- Data buffers are shared-memory IPC objects (§4): the class driver writes/reads
  its own buffer; the HCD maps it for the DMA transfer. No class driver ever
  sees a controller register or another interface's buffer.
- Every URB is validated by the HCD before it touches a ring (endpoint belongs
  to this interface, length within the buffer, direction legal); it fails
  closed (§5.4). Parsing of device-supplied descriptors during enumeration is
  the HCD's job and is bounded/fail-closed (it already is, in `lib/usb`).

### 1.4 Device manager hot-removal reaction

- `devmgr` records, for each driver it loads, the `node_id` it bound to (it
  already learns this when it issues `StoreRequest::Load { bundle_id, node_id }`).
- On a hardware-tree generation bump (`hw_tree_wait` wakes it), it diffs the
  live tree against its bound set. A bound node that has **disappeared** means
  the device is gone; the manager unloads that driver through a new kernel
  **driver-unload mechanism** (§1.5), drops the binding record, and logs the
  unbind with a stable event id (mirror of the load log, §19.4).
- This reaction is generic: it fires for *any* vanished bound node, whatever
  emitted it (the HCD's port-watch, a future bus hot-remove, manual teardown).
  It does not know about USB.

### 1.5 Kernel driver-unload mechanism

- The driver-store server (`StoreRequest`) gains an `Unload { handle }` opcode
  (the symmetric partner of `Load`). The kernel tears the driver process down:
  reclaims its grants (MMIO maps, DMA regions, IRQ bindings, served endpoints),
  removes it from the spawn/loader registry, and emits the unbind audit event.
  Teardown fails closed and is idempotent (unloading an already-gone handle is a
  benign `NotFound`, never a panic, §2.9).
- Capability-gated exactly as load is (`CAP_DRV_LOAD`); the device manager owns
  *policy* (which handle, when), the kernel owns the *mechanism*. No ambient
  authority (§4).

---

## 2. Why this is secure, modular, and reviewable

- **Least privilege per layer (§5.4).** The class driver holds no controller
  register grant and no DMA grant — only the right to submit URBs on its one
  interface. A compromised keyboard driver cannot reprogram the controller,
  reach another device's buffers, or touch the bus.
- **One owner per controller.** Exactly one process maps a given xHCI BAR, so
  there is never a defined-ownership gap or a DMA-ring race between processes.
- **No sibling hardwiring (§2.20, §17.4).** The HCD names no class driver and
  no bus; class drivers name no controller and no bus; bus drivers name no USB.
  Every edge is discovery (`hw_emit_node`) + match (`lib/devmatch`) + the public
  URB ABI. Adding a USB mouse is a new class-driver bundle and zero edits
  elsewhere; supporting USB behind a different controller is a new HCD bundle.
- **Hotplug is structural, not bolted on.** Connect → enumerate → emit →
  autoload; disconnect → `hw_remove_node` → devmgr unload. Both directions use
  the same generation-bump path the device manager already runs (§18.4).
- **Fail-closed throughout (§5.4, §2.9).** URB validation, descriptor parsing,
  unload teardown, and the port-watch all reject/deny on error; none panics.

---

## 3. Staged increments

Each increment ends green on the **whole-project** validation gate (§7). Live
USB attach/detach is metal-only on the Pi 4 (QEMU cannot model the VL805,
`plans/PI.md` §0.4), so the increments are ordered so that everything *except*
the live controller behaviour is host- and CI-proven first.

- **U1 — kernel driver-unload mechanism + devmgr unload reaction `[x]` (DONE).**
  `StoreRequest::Unload { handle }` (+ status-only reply) lives in
  `lib/abi/src/driver_store.rs`; the endpoint request cap is now
  `MAX_REQUEST_LEN`. The kernel teardown is the symmetric partner of
  `spawn_driver_process`: `InitSpawnCtx::terminate_driver_process(handle)`
  (kernel/core, implemented by `KernelInitSpawner`, which gained the IRQ-table
  borrow) reaps the driver's scheduler task (`scheduler.exit` drops its
  control block → kernel stack, live address space, page-table frames), then
  withdraws its address-space-registry entry (grants/streams/limits/matched
  node), destroys its served endpoints (+`call_wake`), releases its IRQ
  bindings, and drops its capability record — idempotent, fail-closed
  (`NotFound` for an already-gone handle), audited (`AuditEvent::DriverUnloaded`,
  4033). The bin reaches it through `DriverProcessSpawn::terminate_driver` and
  the driver-store server serves `StoreRequest::Unload` via `unload_reply`. In
  `devmgr`, `AutoloadState` gained a `node_id → NodeDriver{bundle_id,handle}`
  binding map (recorded in `match_and_load`); `service::react_once` runs
  `autoload::unload_vanished` after each match pass, tearing down (via
  `store::unload_driver`) any bound node absent from the new snapshot — only
  when its driver's *last* bound node has vanished — and purging the
  loaded-bundle cache so a re-attach reloads (`NODE_UNLOADED`, 13_008). Covered
  by host unit tests (ABI round-trip/fail-closed; kernel teardown against the
  real `KernelState` registries; server serve; store unload round-trip;
  `unload_vanished` vanish/present/shared-last-node/fail-soft; service-level
  vanish→unload, no-vanish→none, vanish-then-reattach→reload) and the
  `tests/integration/driver_unload_qemu_aarch64` `-M virt` vertical
  (autoload → `terminate_driver_process` → assert live-task count 1→0 + caps
  /aspace reclaimed + idempotent `NotFound`). Whole gate green.
- **U2 — URB transport ABI + `lib/usb` transport server/client `[x]` (DONE).**
  The wire contract lives in `lib/abi/src/usb_urb.rs`: `UrbRequest` (endpoint,
  `UsbTransferType`, `UsbDirection`, shared-buffer handle, length, control
  SETUP; fixed `URB_REQUEST_LEN`) with fail-closed `decode` (truncation,
  unknown type/direction, endpoint > `MAX_ENDPOINT`), and a status-framed
  completion (`encode_completion`/`encode_error_completion`/`decode_completion`,
  bytes transferred or an in-band `Errno`). It is a driver↔driver IPC format
  (like `driver_store`/`mailbox_ipc`), so it is not part of the C-header
  surface (`cargo xtask c-header` produced no `include/` diff). `lib/usb` gained
  the `transport` module: the `UrbEngine` controller-side seam (`UsbDevice`
  implements it — `control_in` over the EP0 control transfer, `interrupt_in`
  over the `ReportSource` poll), `serve_urb` (decode → validate against the
  interface fail-closed → drive the engine over the shared buffer → frame the
  completion in band; a not-yet-arrived report is `WouldBlock`), and the
  class-side `UrbCall`/`UrbClient` (`control_in`/`interrupt_in` build, submit,
  and decode). Covered by host unit tests (ABI round-trip/fail-closed; the URB
  decoders added to the `lib/abi` fuzz harness; a control-IN + interrupt-IN
  round-trip through `UrbClient` → `serve_urb` → mock engine over a shared
  buffer; and `serve_urb` fail-closed for a bad endpoint, oversize length,
  illegal direction, bulk, and a malformed frame, each proven not to reach the
  engine). Whole gate green.
- **U3a — per-endpoint URB-transport grant mechanism `[x]` (DONE).** §1.3
  requires the right to submit URBs for an interface to be "minted kernel-side
  from the matched node, never ambient" — a mechanism that did not exist. It
  now does, modelled on `msi_alloc`'s allocate-then-grant pattern and reusing
  the existing region-scoped grant machinery, so no `kernel/ipc` change was
  needed:
  - `CapabilityId::IPC_ENDPOINT` (28) is the generic "participate in
    per-endpoint-granted call IPC" capability; `HwResourceKind::Endpoint` (5)
    + `HwResource::endpoint(id)` is the per-endpoint grant (its `base` is the
    call-endpoint id, `covers` is exact-id containment like an IRQ line, its
    required capability is `IPC_ENDPOINT`). The C header regenerated
    (`TAIRIX_CAP_IPC_ENDPOINT`).
  - A *grant-restricted* endpoint is one whose required send caps include
    `CAP_IPC_ENDPOINT`. `call_create` mints the creator the matching
    `HwResource::endpoint(id)` grant when it binds such an endpoint, so the
    server may forward the endpoint onto a node it emits (`hw_emit_node`'s
    existing coverage check admits it unchanged) and the autoloaded class
    driver inherits the grant from its matched node like any MMIO/DMA/IRQ
    resource. `ipc_call` denies a grant-restricted endpoint fail-closed unless
    the caller's grants cover the endpoint id — so two class drivers behind
    one controller cannot reach each other's transport endpoint even though
    both hold the class capability. Covered by host unit tests (ABI
    kind/`covers`/round-trip; kernel `call_create` grant-mint, `ipc_call`
    denied-without-grant, and round-trips-with-grant).
- **U3a2 — cross-process shared-memory primitive `[x]` (DONE).** The URB data
  path needs a shared-memory buffer the class driver owns and the HCD maps
  (§1.3); TAIRiX had only per-process `mem_map`. The kernel now provides a
  generic, capability- and grant-scoped shared-memory primitive (Option B:
  the buffer is plain cacheable RAM with **no** DMA properties, so a class
  driver holds zero DMA authority and the HCD will bounce-copy into its own
  DMA ring — smallest attack surface, no IOMMU coupling):
  - `CapabilityId::SHM` (29) gates participation; `HwResourceKind::Shared`
    (6) + `HwResource::shared(id)` is the per-region grant (exact-id `covers`
    like the endpoint grant, required capability `SHM`). Three syscalls
    `shm_create` (40) / `shm_map` (41) / `shm_unmap` (42); C header
    regenerated (`TAIRIX_CAP_SHM`, `tairix_sys_shm_*`).
  - `shm_create` allocates a physically-contiguous, **zeroed** kernel-owned
    region, maps it cacheable `RW`/non-exec/guard-bracketed into the caller's
    own live space (a fourth per-task window beside MMIO/anon/DMA, reusing the
    guarded-window mechanism with cacheable flags — `MmioWindowMap::
    map_cacheable_into`), records it against the owner, and mints the owner
    `HwResource::shared(id)` so it can forward the region onto an emitted node
    (`hw_emit_node` coverage unchanged). `shm_map` resolves the inherited
    grant against the calling task (forgery/wrong-kind fail closed), maps the
    *same* frames into the grantee, and refcounts. The region's frames are
    **scrubbed and freed only at the last reference** (`shm_unmap`, exit, or
    the U1 driver-unload teardown), so two class drivers behind one controller
    cannot reach each other's buffer and a hot-removed driver's frames are
    zero-on-freed even though the teardown runs in the device manager's
    context (the facility scrubs through the arch direct map, new
    `KernelArch::direct_phys_map`). Mechanism: `kernel/core` `SharedRegion`
    registry (`sharedreg`, facility-as-parameter — no global) + the
    `SharedMemFacility` seam over `LiveSharedMem`; per-arch direct-map wiring
    in all three ports. Covered by host unit tests (ABI kind/`covers`/round-
    trip; `LiveSpace::map_shared`/`unmap_shared`; sharedreg refcount + last-
    ref free + reclaim + fail-closed; shm handler grant-mint/id-write,
    forged/wrong-kind, no-facility).
- **U3a3 — wait-set multi-event wait primitive `[x]` (DONE).** The HCD is a
  single async event loop (§1.1) that must wake on *either* an incoming URB
  IPC call *or* its controller interrupt, for arbitrarily many interfaces.
  TAIRiX had no multi-source wait (`call_recv` parks on one endpoint,
  `irq_wait` on one line), and after the split the class driver holds no
  controller IRQ to park on — so without this it could only busy-poll
  (forbidden, §2.23) or block one interface behind another. The kernel now
  provides a growable, caller-owned **wait-set** (the scalable `epoll`/`kqueue`
  analogue — membership registered once, no fixed source ceiling, §24.1):
  - `SyscallNumber::WAITSET_CREATE` (43, no args → handle) /
    `WAITSET_CTL` (44, set/op/kind/id/token → errno) /
    `WAITSET_WAIT` (45, set/timeout/token_out → errno). `lib/abi/src/waitset.rs`
    holds the two scalar enums `WaitSetOp` (`Add`/`Del`) and `WaitSourceKind`
    (`Endpoint`/`Irq`); no packed wire format (the values cross as registers),
    so no C-header surface beyond the three syscall stubs.
  - A member names a resource the caller **already holds** — an IPC endpoint it
    serves or an `IrqHandle` it bound. `WAITSET_CTL(Add)` resolves and
    **owner-checks that resource against the kernel-trusted caller before
    recording it** (no ambient authority): an unowned endpoint / unbound IRQ
    fails closed `NotFound`. `WAITSET_WAIT` parks the caller on the shared
    `SERVE_WAITQ` + `IRQ_WAITQ` with the timeout as the deadline (woken by an
    endpoint post, a member line firing, or the timed sweep — never a spin),
    re-arms each IRQ member's line before parking, re-checks each member
    against the caller as it scans, writes the ready member's token through the
    validated user boundary **before** consuming the IRQ edge (a faulting
    `token_out` drops no interrupt), and reports first-ready. Needs no
    capability of its own. Wait-sets are reclaimed on task exit and by the U1
    driver-unload teardown.
  - Mechanism: `kernel/core/src/waitset.rs` (growable, handle-keyed registry,
    owner-checked, global pure-data behind a `SpinLock` like `callreg`);
    `CallEndpoint::has_pending` (non-consuming readiness peek, drained by
    `call_recv`); reuses `IrqTable::line_for`/`ready_for`/`try_wait_step`.
    Covered by host unit tests (registry create/add/remove/duplicate/owner-
    check/release; handler owner-checked ctl, timeout, fired-IRQ readiness +
    edge-consume + token-write, pending-endpoint readiness drained by recv).
    `lib/rt`/`lib/drvrt` wrappers + a QEMU vertical land with U3b (the first
    consumer); not added speculatively (§2.4).
- **U3b — xHCI HCD process `[x]` (DONE, live path metal-only).**
  `drivers/bus/usb/xhci` (`tairix-drv-bus-usb`) is now a `lib`+`Run`-binary
  crate: it binds `usb,xhci` (`BIND_KEYS = compatible(XHCI_COMPATIBLE)`, the
  role `lib/hid::KEYBOARD_BIND_KEYS` held), owns the controller, enumerates,
  emits one per-interface node, and serves the URB transport. The host-testable
  logic is in the `lib` target:
  - `bringup` (moved from the deleted `lib/hid::service`, returning the raw
    `UsbDevice` rather than a decoded `BootKeyboard`): `derive_controller_
    resources` + `bring_up_controller[_diagnostic]`, host-tested over mocks up
    to the controller hand-off (`Xhci::open` fails closed on an inert window —
    the metal boundary).
  - `serve`: the alloc-free `UrbService` (≤1 outstanding interrupt-IN URB,
    second concurrent submit → `AlreadyExists`, driven on submit/IRQ via
    `lib/usb::drive_urb`/`frame_completion`, rejecting submits with `NotFound`
    while no interface node is live) + `attach_transport_grants` (adds the
    endpoint + shm grants onto the `describe_device` node). Host-tested.
  - `main.rs` (freestanding): `from_grants_query` → bring-up →
    grant-restricted `call_create` (one protocol-sized endpoint-id block
    claimed by its base id; per-index ids and `shm_create` buffers are
    bound lazily as device-table indices first serve) →
    `describe_device`/`attach_transport_grants`/`hw_emit_node` (now returns the
    assigned node id) → enable interrupter + `irq_bind` → **async wait-set
    loop** (URB endpoint + controller IRQ): recv → `UrbService::on_submit`
    (reply now or hold); IRQ → ack + hotplug service first (hub status-change
    or root-port `CCS`; disconnect aborts the parked URB with `NotFound`) →
    `on_event` only when no disconnect was handled. If a hub-downstream unplug
    appears first as the watched device's failed interrupt transfer, the HCD
    confirms the hub port is disconnected before retracting the node and
    replying `NotFound`. It leaves the hub connection-change latch for the
    status endpoint to report, so an already-stashed or delayed disconnect
    notification drains/re-arms the watch before the later reconnect; live-device
    report faults are not reclassified.
    Bounce-copy is internal to the engine's `interrupt_in` writing the report
    into the HCD's shm mapping; the class driver holds no DMA grant. `lib/rt`
    `shm_*`/`waitset_*` wrappers and `lib/drvrt` `urb_endpoint()`/`map_shared()`
    landed as the consumers.
  - `hw_emit_node` was evolved to **return the kernel-assigned node id** (the
    emitter cannot choose it but needs it to `hw_remove_node` on disconnect);
    `drvrt`/existing bus drivers treat `≥0` as success unchanged.
- **U4 — `usb_kbd` as a pure HID class driver `[x]` (DONE).** Rewritten as a
  `lib`+`Run` crate binding the HID boot-keyboard **interface** key
  (`HwMatchKey::usb(0,0,0x03_01_01)`, in its own `lib` `BIND_KEYS`), holding no
  MMIO/DMA/IRQ — only `CAP_INPUT_INJECT`/`CAP_SHM`/`CAP_IPC_ENDPOINT`/
  `CAP_LOG_EMIT`. It `map_shared`s the granted buffer and runs `pump_once` over
  a `UrbReportSource: ReportSource` that submits a **blocking** interrupt-IN URB
  (`UrbClient` over `ipc_call`) and copies the report out of the shared buffer —
  event-driven, parking in the kernel between keystrokes (§2.23). `NotFound`
  from the URB transport is terminal: the old class-driver process exits so a
  re-emitted node loads a fresh instance instead of retrying a vanished
  interface. The
  controller bring-up is deleted (§2.14); the redundant never-shipped
  `drivers/input/usb_hid` stub is deleted. Image builder ships both the
  `xhci` HCD bundle (`bus_usb/xhci`) and the retuned `usb_kbd` class-driver
  bundle.
- **U5 — event-driven hot-plug + re-enumeration `[x]` (DONE host/CI; live
  metal acceptance is the operator's).** Both staged refinements are built in
  `lib/usb`, and the HCD services them:
  - **Hub-downstream hot-plug is event-driven, not polled.** The `UsbDevice`
    engine, when it descends through a hub, configures and **arms the
    hub's interrupt-IN status-change endpoint** (USB 2.0 §11.12.3) and keeps
    the hub addressed (the resting active control context) concurrently with
    the downstream device. The shared xHCI event ring is **demultiplexed per
    endpoint** (`hub_async_index`/`report_async_index` + the per-device and
    per-hub parking slots), so a status-change report and a keyboard report
    never collide and a synchronous EP0/command wait never faults on an async
    completion. `next_hub_change` reads the changed downstream port: a
    disconnect frees the device slot (Disable Slot) → `HubEvent::Detached`; a
    connect enumerates a **brand-new** device → `HubEvent::Attached`. No
    busy-poll, no spin (§2.23) — it parks on the controller interrupt.
  - **Every port-change latch is drained, not just connect.** Resetting a
    downstream port during enumeration latches the hub's Reset-change (and may
    latch Enable-change) in `wPortChange` alongside Connect-change; a hub keeps
    its status-change endpoint asserting a report for that port until **all**
    its latched changes are cleared. So `clear_hub_port_changes` issues one
    `CLEAR_FEATURE` per set change bit (connection/enable/suspend/over-current/
    reset), and enumeration drains the whole set before the watch is armed.
    Clearing only Connect-change left the Reset-change latched, so the
    freshly-armed watch fired immediately and forever on a stale change — on
    metal that derailed the keyboard (the hub status-change service faulted
    `DeviceFault` and the keyboard's reports errored out). `process_hub_change`
    acts only on a genuine connect/disconnect transition and drains every
    latch, so the watch goes quiet until the next real hot-plug.
  - **Re-attach is a fresh device, never reused state** (the operator's
    requirement): on connect the engine resets the downstream port and
    enumerates on a fresh slot; the HCD re-emits a **new** interface node
    carrying the same endpoint+shm grants, so `devmgr` re-autoloads `usb_kbd`
    onto the same transport and keystrokes resume to the same OS sink.
    Re-attach zeroes the reused EP0/interrupt ring regions first (stale TRBs at
    the producer cycle would otherwise be consumed past the new enqueue
    pointer — a real-hardware correctness fix). A disconnect also aborts any
    parked interrupt-IN URB with `NotFound` before draining a stale faulting
    transfer completion from the vanished device. If the fault arrives before a
    hub status-change completion, `UsbDevice::detach_if_watched_device_gone`
    reads the watched hub port and only then converts the stale transfer into
    interface retraction + `NotFound`, deliberately leaving the hub latch for
    the status endpoint. If the report path had already stashed the hub
    status-change completion, or if it arrives just after retraction, the HCD
    drains it to re-arm the status endpoint before the physical reconnect.
    Ordinary live-device report faults stay visible as transfer errors.
    Old-driver submits that race in after retraction while no interface is live
    are rejected, so the reloaded class driver cannot be blocked by a stale
    ticket from the vanished instance. A freed device slot is retained as
    `UsbDevice::freed_slot` so a **trailing** transfer completion the controller
    still posts for the gone slot (a dropped in-flight transfer, or a Disable
    Slot side-effect) is drained by the event-ring consumers
    (`stash_async_event`/`poll_hub_completion`) rather than faulted — without
    this such an event matched no live endpoint and faulted the hub watch,
    silencing it so a later re-plug went unseen. The tolerance is cleared once a
    fresh device enumerates.
  - **Root-port hot-plug is a per-port CSC scan, uniform for every port.**
    A connect or disconnect on any root port latches `PORTSC.CSC` (and
    posts the Port Status Change Event that raises the interrupt);
    `UsbDevice::next_root_change` — called by the HCD on every interrupt
    wake, before the hub-watch service — reads each port's latch, consumes
    it (`Xhci::clear_port_connect_change`, RW1C-masked), re-reads the live
    connect state, and reconciles: a new connect on an unserved port is
    attached in place (`UsbDevice::attach_root_port` — a hub tier
    installed, descended, and watched, or a leaf served), and a disconnect
    detaches exactly what that port carried (a hub tier cascades with
    everything behind it). The controller is **never** reset for routine
    hot-plug, so sibling ports' devices are untouched; the latch scan is
    register-confirmed, so a Port Status Change Event consumed by an
    engine wait can never lose a plug. `reset_and_reenumerate` (full
    HCRST + re-program + full re-walk) remains only as the latched
    controller-fault recovery. Fail-soft per port, mirroring the hub scan:
    a failed attach consumes the latch (no re-fire storm), snapshots
    `last_attach_fault` (with the root port number; `port_status` is `0` —
    a root port has no hub-format `wPortStatus`), and the first failure is
    surfaced for the HCD's breadcrumb log.
  - **Every reachable device is served concurrently — on every root
    port.** `UsbDevice::bring_up` powers all root ports, parks through the
    connect-debounce window, then attaches **every** connected root port
    (`attach_root_port`, fail-soft per port): a hub tier (the Pi 4's
    onboard USB2 hub) is installed, descended — every connected downstream
    port, nested tiers included — and watched, and a directly-attached
    device (the Pi 4's USB3 side of each jack is wired straight to a root
    port) is served beside it. Every attachment is enumerated on its own
    claimed device-table entry and demand-allocated DMA region — the
    single-root special case (the shared-chunk `enumerate_hid` path and
    the engine-level `root_port`/`root_speed`) is deleted; a hub records
    its root port in its `HubState` (children inherit it into their slot
    contexts) and a leaf in its `DeviceState`, which also lets
    `detach_if_device_gone` confirm a *direct* device's removal on its
    root port's live `CCS`. The resting control cursor generalises with it
    (`rest_active_context`: lowest live hub, else a live device, else the
    idle layout binding — never a released region). Devices fill a
    growable table bounded only by the
    controller's reported slot count and genuine memory exhaustion — a
    keyboard and a storage stick plugged in together are both served, neither
    displacing the other (the Pi 4 boot defect where a plugged-in stick won
    the single device slot and the keyboard never enumerated). Each device
    owns its own demand-allocated DMA region (EP0/interrupt/bulk rings and
    buffers), grown on attach and released on detach, and is
    driven through the per-device `engine_for(index)` `UrbEngine` view; the
    HCD serves one URB transport (endpoint + shared buffer + node) per device
    index, so one interface's transfers can never reach another device's
    endpoints. A port whose device fails enumeration is skipped with its slot
    released, never allowed to cost the other ports their service.
  - **A device absent at initial bring-up is a first-class state, not a
    failure.** The controller is always brought up and left serving. When the
    root device is the onboard hub but no downstream device is connected yet,
    the hub's status-change watch is armed; when no root device is present at
    all, the controller waits for the first root-port connect, which the
    `next_root_change` scan attaches with no reset. The HCD
    publishes an interface node per device actually enumerated
    (`device_live(index)`), so a cold boot with nothing plugged in works:
    plug a device in afterwards and it autoloads. `reset_and_reenumerate`
    re-runs the same bring-up walk, so devices that vanished before
    re-enumeration leave the controller awaiting them rather than faulting.
    A hub claims its own device-table entry's region for its contexts
    (exactly as a nested hub always did), so the leaf devices behind the
    Pi 4's onboard hub occupy indices above it — index 0 is the hub's.
  - Host/CI-proven over the register-level mock (the mock gained a hub
    status-change endpoint, per-slot EP0-ring tracking, Disable Slot, HCRST
    state reset, `PORTSC.CSC` write-1-to-clear, and a root-port-keyed
    fixture model — the hub on root port 1, a leaf on any other — with the
    root hub's live slot tracked for the TT check; `SET_FEATURE(PORT_RESET)`
    latches the Reset-change bit and
    `CLEAR_FEATURE` clears only the selected change, mirroring real hardware):
    `hub_watch_arms…`, `hub_watch_retracts_a_disconnected…`,
    `hub_watch_reenumerates_a_reattached_device_on_a_fresh_slot`,
    `reset_and_reenumerate_brings_up_a_directly_attached_device_as_new`,
    `enumeration_drains_every_port_change_latch_so_the_hub_watch_stays_quiet`,
    `trailing_freed_slot_transfer_event_is_drained_not_faulted`,
    `hub_assembly_unplug_at_root_port_tears_down_and_replug_reenumerates`,
    the multi-root cases (the Pi 4 silent-insertion defect)
    `a_device_plugged_into_a_second_root_port_is_served_while_the_hub_stays_watched`
    and `bring_up_serves_a_hub_tier_and_a_direct_root_device_together`,
    the cold-boot-no-device cases
    `bring_up_keyboard_arms_the_hub_watch_when_no_downstream_device_is_present`,
    `bring_up_keyboard_then_a_downstream_connect_enumerates_a_fresh_keyboard`,
    `bring_up_keyboard_comes_up_awaiting_a_connect_when_no_device_is_attached`,
    and the concurrent-device cases (the mock scripts a keyboard and a
    mass-storage stick on separate hub ports at once, with per-endpoint slot
    attribution)
    `bring_up_serves_a_keyboard_and_a_storage_stick_behind_the_hub_together`
    and `unplugging_the_keyboard_leaves_the_storage_stick_served`.
  - **Whole-hub-assembly unplug (hub directly on a root port) is detected at
    the root port.** When a hub sits directly on a root port and is itself
    pulled, the unplug surfaces as that root port clearing its connect bit, not
    as a downstream hub-port status-change (the hub is gone, so it answers
    neither its status-change endpoint nor a `GET_PORT_STATUS`). The
    `next_root_change` scan sees the latched change, cascades the tier down
    (`detach_hub`: every device and deeper tier behind it), and the HCD
    reconciles its nodes; the later re-plug is attached afresh by the same
    scan — hub reinstalled, descended, watched — with no controller reset,
    so sibling ports' devices are untouched.
  - **Downstream keyboard unplug behind a *persistent* hub is detected on the
    device's own fault code.** The Pi 4 keyboard hangs off a hub that stays
    plugged in, so on unplug the root port keeps reading connected
    (`root_conn=1`, the root-port check above is correctly inert) and the
    disconnect surfaces *only* as the keyboard's own interrupt-IN transfer
    faulting. The metal capture identified that fault as completion code `0x24`
    (xHCI Split Transaction Error) — the hub's transaction translator can no
    longer reach the gone low/full-speed device — while the hub
    `GET_PORT_STATUS` confirmation is unreliable there (it times out,
    `reject_hex=4`). A code that means *the device failed to answer a
    transaction* (`CompletionCode::indicates_device_unreachable`:
    `UsbTransactionError` or `SplitTransactionError`, excluding a stall/babble
    where the device is responding) is conclusive on its own, so
    `UsbDevice::detach_if_device_gone` frees the device slot
    **directly** on that code — captured in `last_report_fault_code` by
    `decode_transfer_report` and read before any hub control transfer — instead
    of depending on the confirmation the vanished device's hub often cannot
    answer. A non-conclusive fault code still falls back to the hub
    `GET_PORT_STATUS` port read.
  - **The teardown is best-effort and never blocks on the controller.** The
    metal capture then showed the *teardown itself* timing out (`reject_hex=4`):
    `detach_downstream_device` issued a Disable Slot command and waited for its
    completion, but the gone device's hub does not let the controller post that
    completion in time, so the teardown returned `DeviceFault`, the slot was
    never freed (its table entry stayed live), and `process_hub_change`
    ignored the re-plug connect (it enumerates only when no device is tracked) —
    the "no log on re-plug" symptom. `UsbDevice::disable_slot_best_effort` now
    posts the Disable Slot, waits within budget, and **frees the local slot
    state regardless of whether the controller confirms** — retiring the
    command-ring slot either way so the ring stays consistent for the next
    enumeration. A late
    Disable Slot Command Completion for the freed slot is drained as a freed-slot
    event by the event-ring consumers (`await_event_for`/`poll_hub_completion`)
    rather than faulting the hub watch. The acted-on fault code is cleared on
    teardown and on a fresh enumeration so a re-plugged device is never
    immediately re-detached; the HCD then re-arms the hub watch, and the hub's
    connect change re-enumerates a fresh keyboard. Host regressions:
    `split_transaction_fault_detaches_without_a_hub_status_confirmation`,
    `split_transaction_detach_frees_the_slot_even_when_disable_is_never_confirmed`,
    `rejected_report_records_its_completion_code_surviving_a_later_control_transfer`.
  - **A failed status-change service never silences the watch.**
    `UsbDevice::next_hub_change` re-arms the hub's status-change interrupt-IN
    endpoint (a fresh transfer + doorbell) **even when `process_hub_change`
    returns an error**, then surfaces that error. Right after a downstream
    disconnect the gone device's transaction translator can briefly fail a hub
    class control transfer (the `reject_hex=4` timeout), so servicing that
    report faults; previously the re-arm was skipped on that error, leaving the
    status-change endpoint with no outstanding transfer — the hub could then
    never post another report and the **re-plug produced no interrupt at all**
    (the engine never woke again — the captured "reconnect not detected"
    symptom). Re-arming first keeps the watch live so the next genuine report
    (the reconnect) still wakes the loop. Host regression:
    `a_failed_status_change_service_re_arms_the_watch_so_a_replug_is_still_seen`.
  - **Every engine wait parks, and downstream hot-plug is interrupt-driven
    only — no periodic port sweep.** The engine's synchronous completion
    waits (`await_event_for`) and the boot-time root-connect debounce park
    on the `EventWait` seam — on metal the HCD's `irq_wait` on the
    controller's bound interrupt line, which is bound **before** the
    controller is started (`UsbDevice::start` itself enables the completion
    interrupter, so cold boot and every post-reset re-program share one
    definition) — and are bounded by **wall-clock** budgets
    (`AWAIT_EVENT_BUDGET_US`, the USB 2.0 §9.2.6 request ceiling;
    `CONNECT_WINDOW_US`, power-on-good + attach debounce), never by an
    iteration-count spin. The hub's status-change interrupt-IN watch is the
    sole downstream hot-plug wake source; the HCD event loop's wait is
    unbounded with no periodic wakes. Every changed port still goes through
    the one `reconcile_hub_port` decision, keyed on the port's *live*
    `GET_PORT_STATUS` state against the tracking tables (attach the
    untracked connected, free the tracked disconnected — hubs cascade — and
    drain stale latches). The earlier "the hub raises no interrupt for a
    downstream connect" metal observation is treated as **undiagnosed**
    until re-captured against the parked-wait engine: a bring-up failure now
    logs the whole breadcrumb (phase, error, open stage, `USBCMD`/`USBSTS`,
    enumeration stage, completion/event-type/reject codes, `PORTSC`), a
    successful bring-up logs a topology summary (devices served, hub watch,
    root port), and a connected device that fails enumeration and is
    skipped logs a warning with the skip count
    (`UsbDevice::skipped_port_count`) instead of looking like an empty
    port. Host regressions:
    `a_missing_completion_times_out_by_wall_clock_and_parks_instead_of_spinning`,
    `an_empty_root_hub_parks_through_the_connect_window_and_reports_not_found`,
    `start_enables_the_interrupter`.
  - **A downstream-port reset is polled to completion, and a failed attach
    keeps its evidence.** Hot-plugging an (empty) external hub into the
    integrated hub faulted the attach (`hub status-change service failed
    err_hex=a`) with nothing in the log to say why. Two fixes:
    - The attach used to wait one fixed 50 ms and then require the port
      enabled in a single `GET_PORT_STATUS`, but a slow external hub
      legitimately takes hundreds of milliseconds to complete a downstream
      reset. `await_port_reset_complete` now re-polls the port status at
      20 ms parked intervals (bounded at 800 ms, the budget production
      stacks allow), requires reset-signalling done **and** the port
      enabled, and settles `TRSTRCY` (10 ms) before the device is
      addressed — a fast hub costs one poll, a slow one is no longer
      refused as a `DeviceFault`.
    - A failed attach snapshots its diagnostics before the best-effort
      latch drain overwrites the live state
      (`UsbDevice::last_attach_fault`: port, error, enumeration stage,
      completion/event-type/reject codes, the port's final observed
      `wPortStatus`; first failure of a service kept, cleared on each
      service/walk entry). The HCD's `hub status-change service failed`
      warning logs that snapshot plus `USBSTS` (or the live diagnostics
      for a non-attach failure), and every error URB reply is logged with
      its errno and the endpoint's latched raw completion code, so a
      collateral class-driver fault (the keyboard's warning beside the
      hub plug) is attributable to the controller's actual verdict.
    Host regressions:
    `a_slow_hub_port_reset_is_polled_until_it_completes`,
    `a_port_that_never_enables_records_its_stage_port_and_final_status`.
  - **A controller-fault recovery can no longer be wedged by a dead class
    driver's leftover URB submit.** Mid-typing (no plug/unplug) the VL805
    latched a controller fault; the HCD's reset/re-enumerate recovery
    worked, but the *reloaded* keyboard driver's every submit was refused
    `AlreadyExists` and it exited fail-closed. Cause: the old driver's
    final `ipc_call` was still queued in the kernel when `devmgr` killed
    it; the kernel kept a dead caller's calls, so after recovery the HCD
    received the corpse's URB, held it in the single outstanding slot,
    and the transport was wedged. Fixes, at the correct layers:
    - Kernel: task reclamation now cancels every call the dead task
      posted (`CallEndpoint::cancel_posted_by`, walked over the registry
      from `reclaim_task_resources`; audit event 3051) — a server never
      receives a dead caller's request (`docs/src/architecture/ipc.md`).
    - ABI/runtime: `call_recv` gained fail-closed `CallRecvFlags`
      (`NON_BLOCKING` answers an empty queue with `WouldBlock`); every
      wait-set-driven event loop — the HCD, `usb_msd` (one endpoint per
      LUN), the display service, login's elevation broker — receives
      non-blocking, so a readiness peek whose call was cancelled can
      never park the loop and starve its other sources.
    - Diagnostics: the `controller fault latched` warning (id 4127) now
      carries live `USBSTS`/`USBCMD`, so the next metal capture names
      which fault bit (HSE/HCE/HCHalted) actually latched — the
      spontaneous mid-typing fault itself remains to be diagnosed from
      that evidence.
    Regressions: kernel/ipc `cancel_posted_by_*` (three-state scrub,
    other-poster isolation, silent no-op), kernel/core
    `reclaim_scrubs_a_dead_posters_queued_call`,
    `nonblocking_call_recv_answers_an_empty_queue_with_would_block`.
  - **A stray controller event never silences the hub watch (the
    "controller goes quiet after the first report" fix).** The Pi 4's black
    USB2 sockets sit behind an *integrated* hub (the engine reports
    `hub_watch=1` even with no external hub and the keyboard in a black port),
    so the keyboard is always a downstream-hub device and the hub-watch path is
    correct. The decisive metal capture showed the `usb-hcd: hub status-change
    service failed` warning with `reject_hex=0`, `evtype_hex=0x21`
    (CommandCompletion), `compl_hex=0x1` (Success): since `control()`/`command()`
    are the only callers of `reset_event_diagnostics`, those fields were
    **stale** from the last successful command — proving the fault issued no
    control/command transfer and so came from `poll_hub_completion`, not the
    `GET_PORT_STATUS` transfer earlier hypotheses blamed. `poll_hub_completion`
    failed closed (`_ => Err(DeviceFault)`) on an event it did not model, and
    `next_hub_change` propagated that error at its `?` **before** the
    status-change endpoint was re-armed — leaving it with no outstanding
    transfer, so the hub could never post another report and every later
    keystroke/unplug/replug produced no interrupt (the controller "went
    silent"). The fix makes that opportunistic poll **drain** any event it does
    not model — an informational controller event (device notification,
    host-controller event, MFINDEX wrap), a trailing freed-slot completion, or a
    keyboard report arriving before the previous one is drained — and keep
    scanning, never faulting (the shared event ring is not a security boundary;
    a genuine fault still surfaces synchronously through the control/command
    waits that follow). Host regression:
    `a_stray_controller_event_during_a_hub_poll_never_silences_the_watch`.
  - **The event-ring drain is the *only* writer of `ERDP`; never a standalone
    Event-Handler-Busy clear (the "weird storm as soon as a key is pressed"
    fix).** xHCI Event Handler Busy (`ERDP.EHB`): the controller sets EHB when
    it asserts the interrupt and re-asserts `IMAN.IP` for a later event only
    once software writes `ERDP` (with the EHB bit) to a position **equal to**
    the controller's internal enqueue. `UsbDevice::poll_event` does exactly
    that per event it dequeues (`ack_event`), so EHB is released precisely when
    the ring is genuinely caught up. A *standalone* `ERDP` write performed on
    every interrupt service — including a wake that dequeued **nothing** —
    was the storm: on the non-coherent VL805/PCIe path the MSI can arrive
    before the event TRB's DMA write is visible to the PE, so the drain sees an
    empty ring while the controller's enqueue is already ahead; writing `ERDP`
    there points the controller at a dequeue *behind* its own enqueue, so it
    re-asserts immediately and the loop spins (the capture: `controller IRQ
    woke URB loop` + `hub change serviced, no event` thousands of times at one
    millisecond, `foreign_drained` frozen, only the first keystroke delivered).
    Writing `ERDP` at the *start* of servicing has the same effect; the timing
    was never the cure. The fix removes the standalone clear entirely:
    `UsbDevice::acknowledge_interrupt` clears `IMAN.IP` **only** (called at the
    start, so a completion posted during the drain re-asserts and is not lost),
    and the per-event `ack_event` the drain performs is the sole `ERDP`/EHB
    writer — matching the metal-confirmed bring-up model, which never wrote
    `ERDP` except per consumed event. A genuinely undelivered event is then
    picked up when its DMA lands and the drain consumes it, not by a
    speculative write. Host regression:
    `acknowledge_clears_ip_only_and_a_zero_event_wake_never_writes_erdp`.
    (Metal-only acceptance still required — QEMU models no Pi USB, §0.4.)
  - **The event-ring drain never consumes a cycle-owned but not-yet-landed
    TRB (the "first key then silent" fix).** With only the keyboard attached
    (the two hubs in the log are the VL805's own integrated USB2/USB3 hubs,
    keyboard on USB2), the **first** keystroke worked end to end and then the
    controller went silent — continuously working only while an *external* hub
    injected extra interrupts. The decisive metal capture's end-of-wake
    **interrupter snapshot** showed `erdp_ehb=1` (Event Handler Busy **stuck
    set**) with `IMAN.IP=0`, and `usb-hcd: last drained foreign event` showed
    `trbtype=0` (an all-zero TRB) drained on the first keypress (`foreign_drained`
    0→1). Root cause: `UsbDevice::poll_event` gated consumption on the cycle bit
    alone. On the non-coherent BCM2711/VL805 PCIe path the VL805's 16-byte event
    TRB write does not reach RAM atomically, so the announcing cycle bit can be
    visible to the PE while the body is still the zeroed initial state — the
    `dma_rmb` orders *this PE's* reads (body after cycle) but cannot order the
    *controller's* posted writes into RAM. The drain then popped that phantom
    zero TRB, advanced the dequeue **past** the controller's enqueue, wrote
    `ERDP` there, and permanently desynchronised the consumer cycle: the
    controller next set EHB for a real event whose cycle the (over-advanced)
    cursor no longer recognised, so `owned()` stayed false, EHB stayed set, and
    no further completion interrupt was raised. The external hub's unrelated
    MSIs were the only thing that incidentally re-woke the HCD. (This is the
    same defect class as the earlier storm — the zero-event-wake `ERDP` write —
    here triggered by the *body* lagging the cycle rather than the MSI lagging
    the event.) Fix: after the barrier and a re-confirmed `owned()`, `poll_event`
    refuses to consume an entry whose `trb_type_raw() == 0` (a real event TRB is
    never type 0): it leaves the entry un-consumed, writes no `ERDP`, and the
    next wake re-reads it once the body has landed — no poll, no spin, no stale
    pointer. Host regression:
    `a_cycle_owned_but_not_yet_landed_event_is_not_consumed_until_its_body_arrives`
    (the harness models a cycle-visible/body-zeroed entry via
    `MockXhci::unland_last_event`/`land_last_event`; it faults+over-consumes
    without the guard and is left alone with it).
    Metal-confirmed: with this fix, boot-time typing works end to end and every
    keystroke wake reads a healthy interrupter, exactly as predicted. The
    TEMPORARY metal diagnostics used to localise these faults (the BCM2711
    `brcm-msi` vector diag; the HCD `URB submit received` / `URB held` / hotplug
    / interrupter-snapshot / foreign-event Info logs, and the `UsbDevice`
    drain-count / foreign-event / disable-confirmed accessors that fed them)
    have been removed now that the chain is complete; the load-bearing tolerance
    behaviour they observed remains.
  - **A controller that latches a fatal error / halts is reset and
    re-enumerated, never left silent (the "unplug worked but the re-plug is
    never seen" fix).** With boot typing fixed, the remaining failure was
    purely unplug→replug: the metal capture showed the unplug retract the node
    and complete its Disable Slot (`disable_confirmed=1`), then the end-of-wake
    interrupter snapshot read `usbsts=0x0d`/`0x05` — `USBSTS.HSE` (Host System
    Error) **and** `HCHalted` set, `erdp_ehb` stuck — whereas every keystroke
    wake read `usbsts=0`. The controller *halts itself* during the
    downstream-device hot-removal teardown, **after** the Disable Slot already
    completed (so the controller was alive then — the halt is induced later in
    the teardown, not by the unplug). A halted controller runs nothing and
    raises no further interrupts, so the re-armed hub status-change watch never
    saw the re-plug. Decisive corroboration: a cold boot with the keyboard
    **unplugged** then plugged in works, because that path never runs the
    teardown. Per the xHCI spec a Host System Error clears only with a Host
    Controller Reset, so the fix detects the faulted controller
    (`UsbDevice::controller_faulted` = `USBSTS & (HSE|HCHalted)`) at the end of
    each controller-IRQ wake and recovers via `reset_and_reenumerate` — the
    same full HC reset + fresh enumeration a cold boot performs — returning to
    the proven await-connect state so the re-plug enumerates through the normal
    attach path (`reset_reenumerate_and_publish`, the fault-recovery-only
    reset; routine hot-plug never resets the controller) after both
    disconnect exits (`recover_if_controller_faulted`). Host regression:
    `controller_faulted_reports_hse_and_halt_and_recovery_clears_it` (healthy →
    not faulted; latched HSE → faulted; HC reset clears it; Run/Stop clear →
    HCHalted → faulted). The HCD main-loop wiring is a freestanding binary, so
    coverage is at the lib/usb predicate+recovery level.
    (Metal-only acceptance still required — QEMU models no Pi USB, §0.4.)
  - **A re-plugged device reloads its class driver (the "unplug seen, re-plug
    never reloads" fix).** With the controller recovery above, the HCD
    correctly retracts the interface node on unplug and re-emits it on
    re-plug, but the re-plugged keyboard produced no input because `devmgr`
    never reloaded the class driver. Root cause was in the kernel driver-load
    path, not USB: the user-space load mechanism reported the **driver host's
    per-instance handle counter** (always `1`, since a fresh `Host` is built
    per load) instead of the spawned process id. `devmgr`'s hot-removal diff
    (`unload_vanished`) keys teardown on that handle and skips a driver while
    *any other* bound node shares its handle — so with every driver reporting
    handle `1`, the vanished keyboard's driver was never torn down and its
    loaded-bundle cache never purged, so the re-emitted node reused the stale
    handle and was never re-spawned. (The same handle was also wrong for the
    teardown seam, which resolves it as a PID.) `SpawnDriverLoader::load` now
    returns the unique spawned PID as the driver handle. Host regression: the
    store-server `a_load_spawns_the_matched_signed_driver_with_the_nodes_resources`
    test asserts the reported handle is the spawned PID, not the host counter.
  - **Remaining (operator's):** live metal acceptance — attach → keystroke,
    detach → `usb_kbd` unloaded (controller stays up), re-attach → autoloads
    again, **and cold boot with the keyboard unplugged then plugged in** — is
    inherently metal-only (QEMU models no Pi USB, §0.4). Update the `README.md`
    matrix on metal sign-off.

- **U6 — `usb_mouse` class driver + broken-device isolation `[x]` (DONE;
  live path metal-only).** `drivers/input/usb_mouse` is the HID boot-mouse
  sibling of `usb_kbd`: a `lib`+`Run` crate binding the boot-mouse interface
  key (`HwMatchKey::usb(0,0,0x03_01_02)`), same least-privilege caps
  (`CAP_INPUT_INJECT`/`CAP_SHM`/`CAP_IPC_ENDPOINT`/`CAP_LOG_EMIT`), pumping
  blocking interrupt-IN URBs through `tairix_hid::BootMouse` and injecting
  each decoded event through the one shared `PointerInput::from_device_event`
  mapping → `pointer_inject` (scroll ticks decode but are not injected — no
  pointer-record scroll consumer exists yet). `NotFound` exits for reload;
  repeated faults exit fail-closed. The bundle ships in the Pi image
  (`Drivers/input/usb_mouse/Run`, signed).
  Two engine defects found while landing it (the metal boot log's ~4 s
  `hub status-change service failed err_hex=a` loop with a mouse connected):
  - **A failed downstream attach left the port's latched changes set**, so
    the hub status-change endpoint re-reported the same stale change forever
    and every re-service re-ran the failing multi-second enumeration —
    starving every other port's service (the keyboard died the moment a
    problematic device shared the hub). `attach_hub_port` and
    `attach_downstream_device` now drain the port's whole latch set
    (best-effort) on the failed path too, so a broken device costs one
    surfaced error per genuine change, never a fault loop.
  - **One port's failure aborted the whole hot-plug scan**
    (`process_hub_change` propagated the first attach error with `?`),
    so a broken device's connect change starved a healthy device's event in
    the same report. The scan is now per-port fail-soft, mirroring the
    bring-up walk: the failing port is drained and skipped, the remaining
    changed ports are serviced, and the first failure is surfaced only when
    no actionable event was found.
  Host regressions (the mock gained a scripted boot mouse on its own port
  with a second, slot-attributed interrupt endpoint, and a
  port-never-enables knob):
  `bring_up_serves_a_keyboard_and_a_mouse_behind_the_hub_together`,
  `a_failing_port_at_bring_up_never_costs_the_keyboard_its_service`,
  `a_failed_hot_plug_attach_drains_the_port_latches_so_the_watch_stays_quiet`.

- **U7 — composite (multi-interface) devices `[x]` (DONE; live path
  metal-only).** One physical USB device may carry several functions — the
  motivating hardware is a wireless keyboard+mouse receiver whose single
  configuration holds a boot-keyboard interface *and* a boot-mouse
  interface. Before this, `InterfaceInfo` decoded only the first interface
  (the second function was structurally invisible), the 64-byte
  configuration read truncated a two/three-interface configuration
  mid-descriptor, and one device-table entry existed per device — on metal
  the receiver's attach faulted (`hub status-change service failed
  err_hex=a`) and booting with it connected cost the keyboard its service.
  The engine now realises §1.1's "one node per interface" for composite
  devices:
  - `InterfaceInfo::decode_all` decodes **every** default-alternate
    interface (bounded by `MAX_INTERFACES`, alternate settings skipped, a
    malformed HID interface with no interrupt-IN endpoint dropped so a
    well-formed sibling is still served); the control-data buffer
    (`CTRL_DATA_LEN`, 512 B) holds a composite device's whole configuration
    in one read.
  - `finish_enumeration` plans one device-table entry per servable
    interface: the primary at the caller's index (it owns the slot's parked
    EP0 cursor via `active_device`), each sibling at its own free index and
    ring region while **sharing the device's slot and EP0**. Every
    Configure Endpoint carries Context Entries covering the highest served
    DCI, `SET_CONFIGURATION` runs once, and `SET_PROTOCOL(boot)` runs per
    HID interface. A sibling's control transfers route through the slot's
    EP0 owner (`ep0_owner_index`); the (slot, DCI) event demux and the
    per-index report/bulk paths are unchanged.
  - `detach_device` frees **every** entry sharing the vanished device's
    slot, so one physical unplug retracts all of its interfaces; the HCD
    reconciles all published nodes against the live table
    (`reconcile_interfaces`) after any hub event, fault detach, or
    reset/re-enumeration instead of touching a single index.
  - Host regressions (the mock gained a composite fixture whose 75-byte
    configuration exceeds the old 64-byte read plus an alternate-setting
    decoy, and a `composite_downstream_port` knob capturing the same slot's
    second interrupt endpoint):
    `interface_info_decodes_every_interface_of_a_composite_device`,
    `interface_info_drops_a_malformed_hid_interface_but_serves_its_sibling`,
    `interface_info_bounds_the_decoded_interface_set`,
    `bring_up_serves_both_interfaces_of_a_composite_receiver`,
    `unplugging_a_composite_receiver_frees_both_interfaces_and_a_replug_reserves_them`,
    `a_composite_receiver_beside_the_keyboard_costs_it_nothing`.

- **U8 — EP0 max-packet discovery + exact-length descriptor reads `[x]`
  (DONE; live path metal-only).** Enumeration assumed the speed's
  worst-case EP0 max packet (full speed → 64) for every EP0 transfer, but
  a full-speed device may legally use 8/16/32 — the real wireless
  receiver reports `bMaxPacketSize0` = 8, so its 18-byte device-descriptor
  read terminated short at the first 8-byte packet and the attach faulted
  (`hub status-change service failed err_hex=a` at hot-plug; at boot the
  failed walk left no device served). The engine now performs the
  standard fix-up every production stack does:
  - `finish_enumeration` first reads the 8-byte descriptor prefix (one
    packet at the smallest legal EP0 size, so it completes at any real
    size), validates `bMaxPacketSize0` against the speed
    (`ep0_max_packet_from_descriptor`: low 8, full 8/16/32/64, high 64,
    SuperSpeed exponent 9; anything else `BadMagic`, fail-closed), and
    issues an **Evaluate Context** (`TrbType::EvaluateContext`, A1-only
    input context, xHCI §4.6.7) whenever the honest size differs from the
    Address Device assumption — only then the full 18-byte read.
  - The configuration descriptor is read at its exact advertised length:
    a 9-byte header read for `wTotalLength`, then precisely that many
    bytes (clamped to `CTRL_DATA_LEN`) — never an over-long request a
    buggy device might mishandle.
  - The mock models the physics (per-slot programmed EP0 max captured at
    Address Device / Evaluate Context; a mismatched descriptor read
    delivers one device-sized packet), the composite fixture is a
    full-speed device with `bMaxPacketSize0` = 8 like the real receiver,
    and regressions pin the behaviour:
    `bring_up_serves_both_interfaces_of_a_composite_receiver` /
    `a_composite_receiver_beside_the_keyboard_costs_it_nothing` (exactly
    one Evaluate Context for the receiver, none for the 64-byte keyboard),
    `a_forged_ep0_max_packet_fails_closed_without_costing_the_keyboard`,
    `ep0_max_packet_validation_follows_the_speed_rules`.

- **U9 — multi-tier hubs (a hub plugged into a hub) `[x]` (DONE; live path
  metal-only).** The engine tracks every hub in a growable table
  (`lib/usb::device::HubState`, bounded only by the controller's slot
  count, `MAX_HUB_DEPTH` = 5 route-string tiers — the protocol-fixed
  Route String depth), not a single implicit tier:
  - **Topology is per hub, never assumed.** Each hub records its parent
    hub + port, its Route String, depth, speed, and the TT coordinates its
    slot context carries. A child extends its parent's route by one nibble
    (`route_for_child`, fail-closed on port 0/>15 and on exceeding the five
    tiers), and a full/low-speed child splits through the TT of the nearest
    **high-speed** ancestor: the parent hub itself when high-speed, else
    the coordinates the parent inherited (§6.2.2/§8.9).
  - **A downstream device that enumerates as a hub is installed, marked,
    and descended** (`install_hub` → `descend_hub`): its ports are powered
    and scanned (per-port fail-soft, exactly like the bring-up walk — and
    a tier whose descent fails is torn down whole, never left
    half-installed), and its own interrupt-IN status-change endpoint is
    configured and armed on its own layout region
    (`Layout::hub_regions`). Its slot contexts stay on the device region it
    was enumerated on, which it claims for its lifetime
    (`HubState::device_region`, excluded from `free_device_index`).
  - **Every tier is watched concurrently.** The event-ring demux routes
    each status-change completion to its hub (`hub_async_index`, per-hub
    parking slots); `next_hub_change` services whichever hub reported and
    re-arms that hub's watch even on a failed service. Hub-class requests
    target any hub through the generalised active-control-context
    switching (`activate_hub_control`/`hub_control`; the resting context is
    the root hub).
  - **Unplugging a hub cascades.** The parent's port disconnect tears down
    the hub and everything behind it — served devices and deeper hub tiers
    recursively (`detach_hub`) — as `HubEvent::HubDetached`; a hot-plugged
    hub is installed and descended in place as `HubEvent::HubAttached`.
    The HCD reconciles its published interface nodes against the live
    device table for both, so class drivers autoload/unload per device
    exactly as for a leaf hot-plug.
  - Host-proven over the register-level mock, which gained a **nested-hub
    bank** (`with_nested_hub`: a high-speed hub on the root hub's port 3
    with a full-speed keyboard on its port 2; per-EP0-slot routing of
    hub-class requests, nested marking/status-endpoint capture, nested TT
    validation at Address Device, `post_nested_hub_status_change`):
    `bring_up_serves_a_keyboard_behind_a_nested_hub` (route string `0x23`
    + nested TT + end-to-end report),
    `hot_plug_on_a_nested_hubs_port_attaches_and_detaches_through_its_own_watch`,
    `unplugging_a_nested_hub_cascades_and_a_replug_rebuilds_the_tier`,
    `route_for_child_extends_one_nibble_per_tier_and_fails_closed`.

U1–U9 are landed; the modular USB stack — bus driver → user-space HCD owning
one controller and serving the URB transport → per-interface class drivers
(keyboard, mouse, mass storage), with event-driven hub hot-plug on every
tier, recursive multi-tier hub descent with cascade teardown, fresh
re-enumeration, per-port failure isolation, composite (multi-interface)
devices served one node per interface, and EP0 max-packet discovery for
full-speed devices — is complete and host-/CI-proven. The
live attach/detach/re-attach behaviour is metal-only and is the operator's
acceptance step (QEMU models no Pi USB, §0.4).

---

## 4. Out of scope (explicitly)

- Non-boot-protocol HID and isochronous transfers — each is a later class
  driver or HCD extension on top of this seam, not part of bringing the split
  up. (Hub *hot-plug* is in: U5 services each hub's status-change endpoint
  event-driven, and U9 descends and watches **multi-tier** hubs — a hub
  plugged into a hub — with cascade teardown. Bulk transfers themselves have
  since landed on this seam — `plans/DEVICES.md` D1 — and the mass-storage
  class driver followed as DEVICES.md D2.)
- A second host-controller driver (a non-xHCI controller): the architecture
  admits it (it binds a different controller node and serves the same URB ABI),
  but none is planned here.
