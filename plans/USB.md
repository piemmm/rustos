# USB.md — Modular USB stack and device hot-removal

This is the staged build plan for RustOS's **modular USB stack**: a clean
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
  drivers exist. RustOS may carry several controller drivers and several class
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
  arch-neutral `rustos_hid` composition and injecting keystrokes through the
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
  "transport client" over this ABI; the existing single-device enumeration
  engine is reused by the HCD, not the class driver). The interface node the
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
    (`ROS_CAP_IPC_ENDPOINT`).
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
  (§1.3); RustOS had only per-process `mem_map`. The kernel now provides a
  generic, capability- and grant-scoped shared-memory primitive (Option B:
  the buffer is plain cacheable RAM with **no** DMA properties, so a class
  driver holds zero DMA authority and the HCD will bounce-copy into its own
  DMA ring — smallest attack surface, no IOMMU coupling):
  - `CapabilityId::SHM` (29) gates participation; `HwResourceKind::Shared`
    (6) + `HwResource::shared(id)` is the per-region grant (exact-id `covers`
    like the endpoint grant, required capability `SHM`). Three syscalls
    `shm_create` (40) / `shm_map` (41) / `shm_unmap` (42); C header
    regenerated (`ROS_CAP_SHM`, `ros_sys_shm_*`).
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
  RustOS had no multi-source wait (`call_recv` parks on one endpoint,
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
  `drivers/bus/usb/xhci` (`rustos-drv-bus-usb`) is now a `lib`+`Run`-binary
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
    `lib/usb::drive_urb`/`frame_completion`) + `attach_transport_grants` (adds
    the endpoint + shm grants onto the `describe_device` node). Host-tested.
  - `main.rs` (freestanding): `from_grants_query` → bring-up → `shm_create`
    (the URB buffer) + grant-restricted `call_create` (probed id range) →
    `describe_device`/`attach_transport_grants`/`hw_emit_node` (now returns the
    assigned node id) → enable interrupter + `irq_bind` → **async wait-set
    loop** (URB endpoint + controller IRQ): recv → `UrbService::on_submit`
    (reply now or hold); IRQ → ack + `on_event` (reply the completed URB) +
    root-port `CCS` watch → `hw_remove_node`. Bounce-copy is internal to the
    engine's `interrupt_in` writing the report into the HCD's shm mapping; the
    class driver holds no DMA grant. `lib/rt` `shm_*`/`waitset_*` wrappers and
    `lib/drvrt` `urb_endpoint()`/`map_shared()` landed as the consumers.
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
  event-driven, parking in the kernel between keystrokes (§2.23). The
  controller bring-up is deleted (§2.14); the redundant never-shipped
  `drivers/input/usb_hid` stub is deleted. Image builder ships both the
  `xhci` HCD bundle (`bus_usb/xhci`) and the retuned `usb_kbd` class-driver
  bundle.
- **U5 — end-to-end metal acceptance + cleanup `[ ]`.** Attach → keystroke;
  detach → `usb_kbd` unloaded, controller stays up; re-attach → autoloads
  again. The HCD's disconnect→`hw_remove_node` is wired (root-port `CCS`);
  two refinements are staged here, not yet built: **re-enumeration on
  re-attach** (the `UsbDevice` engine is one-shot; a fresh connect needs a
  reset+re-enumerate path) and **hub-downstream disconnect detection** (the
  current watch is the device's root port; a device behind the onboard hub
  needs the hub's per-port status). Update `README.md` matrix on metal sign-off.

U1, U2, U3a, U3a2, U3a3, U3b, and U4 are landed; the modular split is complete
and host-/CI-proven. U5 (live metal acceptance + the two staged refinements
above) is the remaining increment, and is inherently metal-only (QEMU models
no Pi USB, §0.4).

---

## 4. Out of scope (explicitly)

- Non-boot-protocol HID, hubs beyond the root hub, isochronous transfers, and
  bulk-storage class drivers — each is a later class driver or HCD extension on
  top of this seam, not part of bringing the split up.
- A second host-controller driver (a non-xHCI controller): the architecture
  admits it (it binds a different controller node and serves the same URB ABI),
  but none is planned here.
