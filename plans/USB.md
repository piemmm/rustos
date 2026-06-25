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
- **U2 — URB transport ABI + `lib/usb` transport server/client `[ ]`.**
  Host-testable with a mock controller. Define `lib/abi/src/usb_urb.rs` (URB
  request/completion records, endpoint addressing) and regenerate the C header.
  Refactor `lib/usb` to expose a controller-side transport server (drains URB
  calls, drives the existing ring/enumeration engine) and a class-side
  transport client (submits URBs, awaits completions). Host unit tests over a
  mock ring assert a control + interrupt-IN round-trip and fail-closed
  validation (bad endpoint, oversize length, illegal direction).
- **U3 — xHCI HCD process `[ ]`.** Turn `drivers/bus/usb/xhci` into a `Run`
  binary that binds `usb,xhci`, owns the controller (the bring-up code moves
  from `usb_kbd` unchanged — it is already platform-neutral), enumerates,
  emits one per-interface node, serves the URB transport endpoint, and watches
  root-hub PORTSC → `hw_remove_node` on disconnect. Host stub + the metal
  acceptance for live enumerate/emit.
- **U4 — `usb_kbd` as a pure HID class driver `[ ]`.** Rebind it to the
  emitted HID-interface node, delete the in-process controller bring-up
  (§2.14), and pump reports over the URB transport client. Update its bind
  table (HID boot-keyboard class key, not the xHCI class key) and README.
- **U5 — end-to-end metal acceptance + cleanup `[ ]`.** Attach → keystroke;
  detach → `usb_kbd` unloaded, controller stays up; re-attach → autoloads
  again. Retire any transitional scaffolding and update `PLAN.md` / §3 / §16.4.

U1 is the foundation (hot-removal needs a way to unload), is fully host- and
QEMU-testable, and unblocks the rest; it is the next increment to implement.

---

## 4. Out of scope (explicitly)

- Non-boot-protocol HID, hubs beyond the root hub, isochronous transfers, and
  bulk-storage class drivers — each is a later class driver or HCD extension on
  top of this seam, not part of bringing the split up.
- A second host-controller driver (a non-xHCI controller): the architecture
  admits it (it binds a different controller node and serves the same URB ABI),
  but none is planned here.
