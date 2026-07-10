# Hardware detection and driver autoload (`rustos-devmgr`)

RustOS detects the hardware actually present at boot and autoloads the
matching drivers; it never ships a hand-maintained, per-image static
device list (`AGENTS.md` §18). Three pieces compose:

1. **The hardware tree** (`lib/abi/src/hwtree.rs`): the single,
   architecture-neutral inventory of detected buses and devices. Each
   node carries a stable id, its parent, a device class, its **match
   keys** (device-tree `compatible` strings, PCI `vendor:device:class`,
   USB `vid:pid:class`, virtio device ids), and its resource needs
   expressed as capability-grant requests — never ambient handles
   (`AGENTS.md` §18.1).
2. **Per-architecture discovery** (`AGENTS.md` §18.2): each
   `kernel/arch/<target>/` port normalises its platform's native source
   (FDT, ACPI, host query) into the tree. Architecture-specific parsing
   never leaks past the port.
3. **The device manager** (`userland/system/devmgr`): the user-space
   matching policy this page documents. Matching is not kernel code
   (`AGENTS.md` §4 — microkernel-leaning).

## Matching model

A driver declares the nodes it can drive in the **bind table** of its
signed manifest: up to `DRIVER_MANIFEST_MAX_BIND_KEYS` entries, each a
`DriverBindKey` pairing one `HwMatchKey` with a bind `priority`
(see [driver lifecycle](./lifecycle.md) for the `.rxe` wire layout).
The table is covered by the manifest signature and validated
fail-closed at the load gate, so a malformed entry never reaches the
matcher.

A bind-table entry matches a node when its `HwMatchKey` matches one of
the node's match keys (`HwMatchKey::matches`) — same kind first, then:

* a `Compatible` key matches byte-for-byte;
* a `Virtio` key matches on device id;
* a `Pci` / `Usb` key matches on the class code, and on each of
  `vendor` / `product` either *exactly* or as a **wildcard** when the
  **bind** key leaves that field `0` — so a generic class driver (an
  xHCI host, an HID boot keyboard) binds without hard-coding a device
  id, while a vendor-specific driver still names an exact id and, with a
  higher `priority`, outranks the generic one. Widening is only ever
  requested by the bind key (which is signed); a discovered node can
  never force a broader match (fail closed, `AGENTS.md` §5.4).

Drivers declare their canonical bind tables as a `pub const BIND_KEYS`
in the driver crate (e.g. `rustos_drv_bus_pcie_brcm::BIND_KEYS`,
`rustos_drv_bus_usb::BIND_KEYS`, `rustos_drv_input_usb_kbd::BIND_KEYS`) —
the single source of truth the signed-manifest bind table is authored
from (`AGENTS.md` §18.3).

The resolution across drivers is deterministic (`AGENTS.md` §18.3):

* the candidate holding the **strictly highest** matched priority
  binds the node;
* an unbroken tie between two *distinct* candidates is a packaging
  defect: the node is refused a binding and the defect is audited —
  never a coin-flip;
* ties *within* one candidate's own table are harmless (the same
  driver binds either way);
* a node matching no candidate is left **unbound and logged** — never
  an error and never a panic (`AGENTS.md` §18.4). A headless image
  simply leaves its display node unbound and proceeds to text login.

This resolution policy (`resolve`, `best_bind_priority`, `DriverCandidate`,
`MatchResolution`) lives in the shared **`lib/devmatch`** crate, the single
§18.3 definition. `rustos-devmgr` re-exports it unchanged, and the kernel's
in-kernel bootstrap-floor driver-candidate catalogue
(`kernel/rustos-kernel::driver_catalog` — the storage floor only, §18.6, see
[Remaining work](#remaining-stage-4hw-work)) resolves against the same crate — the
kernel cannot depend on the `userland/*` device manager (`AGENTS.md` §17.4),
so the policy is shared, never duplicated (§2.2).

## Public surface

```rust
use rustos_devmgr::{DeviceManager, DriverCandidate, DriverLoader};

fn autoload_at_boot(deps: &mut ServiceDeps) {
    let candidates: &[DriverCandidate<'_>] = &deps.catalog; // path + decoded bind table
    let manager = DeviceManager::new(&deps.audit_sink);     // impl rustos_log::Sink
    let report = manager.autoload(
        &deps.hardware_tree, // &[rustos_abi::HwNode]
        candidates,
        &deps.caller_caps,   // must hold CAP_DRV_LOAD for loads to pass
        &mut deps.loader,    // impl DriverLoader over the drvhost gate
    );
    // report.bindings / report.unbound / report.ties_rejected /
    // report.load_failures summarise the walk.
}
```

`DeviceManager::autoload` walks the tree in order, resolves each
non-root node as above, and loads the winning driver once **per
node** — each load spawns its own driver instance holding exactly that
node's resource grants, so two identical devices (a virtio keyboard
and a virtio mouse, say) each get a live instance rather than the
second being bound in name only. A load refusal fails only its node,
closed (`AGENTS.md` §5.4); the walk continues so one bad image cannot
block boot.

### The `DriverLoader` seam

The §17.4 layering keeps `rustos-devmgr` on `lib/*` only, so the load
*mechanism* sits behind the `DriverLoader` trait:

```rust
pub trait DriverLoader {
    fn load(
        &mut self,
        path: &str,
        resources: &[HwResource],
        caller_caps: &CapabilitySet,
    ) -> Result<DriverHandle, Errno>;
}
```

The device manager forwards the **matched node's** `HwResource`
requests (`HwNode::resources`) verbatim to the loader, so the load
mechanism can mint the loaded driver exactly the device-resource grants
its node exposed and nothing more (`AGENTS.md` §18.3 — a loaded driver
receives only the resources its matched node requested). The resources
originate kernel-side, from the discovered hardware tree, never from an
untrusted caller (§4 — no ambient authority); the device manager only
forwards them.

The deployment's integration point implements `DriverLoader` over the
[driver host](./host.md)'s `Host::load` pipeline (mapping `HostError`
via `as_errno`), so every load still passes the full §8 gate —
signature verification, `CAP_DRV_LOAD` / `CAP_DRV_KERNEL` checks, and
the spawner hand-off. The device manager never inspects or bypasses
those checks, and it never re-parses image bytes: candidate bind
tables arrive already decoded (fail-closed) by the gate's own
`ParsedImage::decode_bind_table`.

The production process-spawning integration point is
`rustos_kernel::driver_spawn_loader::SpawnDriverLoader`: it runs the
signed `Host::load` gate on the discovered `kind = UserSpace` image and
then spawns the verified payload into its own process through the
architecture `DriverProcessSpawn` seam, minting the new process one
grant per `resources` entry via `KernelSpawnCtx.grants` (the kernel's
own discovered requests, §18.3). The
`tests/integration/driver_spawn_qemu_aarch64` vertical proves the full
`autoload` → signed gate → spawn → grant-delivery path on the `virt`
board (a virtio node stands in for the metal controller, since no
Pi-board QEMU vertical exists).

### Recursive, user-space discovery (`hw_emit_node`)

Discovery does not stop at the nodes the kernel's boot-time walk produced.
A **bus** driver — a PCIe root complex, a USB host — runs in user space too
(§4), enumerates the devices behind it, and publishes each as a child node
into the live hardware tree through the `hw_emit_node` syscall (no. 37,
gated on `CAP_HW_EMIT`; the rt-backed `DriverHost::emit_node`). That bumps
the tree generation, the reactive `devmgr` loop re-reads and re-matches, and
the child's driver is autoloaded in turn — so the *set of loadable drivers*
and the *device topology* both grow at runtime, never from a compiled-in
list (§18, §18.6). The kernel admits a published node only when every
`HwResource` it requests is covered by one of the **emitting driver's** own
minted grants (`HwResource::covers`): a bus driver can hand a child only
authority it already holds, so the recursion can never escalate privilege
(§4 — no ambient authority; §18.3). Coverage is decided per resource kind:
an `Mmio`/`Port`/`Irq` window or line range must lie wholly inside a grant
of the same kind; a `Dma` constraint may be no wider; a `BusWindow`
sub-window must keep the parent's exact CPU↔bus translation. The one
cross-kind rule is the central PCI(e) case: a host bridge holds its
outbound window as a `BusWindow` grant and authorises every CPU access
within it, so it covers a child device's register **BAR** — an `Mmio`
window the bridge's enumeration has already resolved to a CPU-physical
address *inside* that outbound window. The bootstrap floor (§18.6) seeds
only the first nodes needed to reach the driver store; everything below a
discovered bus is published by that bus's user-space driver.

The kernel **owns the published node's identity** — the emitter never names
it (`AGENTS.md` §4 / §5.4 — identity is kernel-provided, never
caller-supplied). A driver builds the node's class, match keys, and resource
requests and leaves `id`/`parent` unassigned (`PciBus::describe_function`
returns a placeholder identity); on publish the kernel (1) assigns a fresh
`id` one past the largest live node id, so an emitter-chosen id can never
collide with an existing node, and (2) sets the parent to the **emitter's own
matched node** — looked up kernel-side from the calling task, recorded when
the driver was loaded for that node — so a driver can neither forge its
position in the tree nor publish a child under a node it was not loaded for.
The unique id is load-bearing, not cosmetic: the driver-store load path
resolves a matched node by its id to mint the loaded driver's grants, so a
collision would mint the wrong driver's authority. A task with no recorded
loaded node (an ordinary process, or a driver not loaded for a node) may
publish nothing — `hw_emit_node` fails closed with `PermissionDenied`.

## Audit surface

Every match, load, skip, and failure is logged through `lib/log` with
a stable event id in the device manager's reserved `13000..14000`
range (`rustos_devmgr::events`):

| Event id | Meaning |
|----------|---------|
| `13001` `NODE_BOUND` | a node was bound; fields: `node`, `path`, `handle` |
| `13002` `NODE_UNBOUND` | no driver matched; never an error (§18.4). Emitted at **`Debug`** — the routine, high-volume case (most nodes have no driver), filtered out by the default `Info` threshold so it never floods the slow diagnostic UART (§20 / §2.16) |
| `13003` `NODE_TIE_REJECTED` | unbroken highest-priority tie refused; field: `priority` |
| `13004` `NODE_LOAD_FAILED` | the load gate refused the winner; fields: `path`, `errno` |

The drvhost gate's own `7000`-range records interleave with these on a
shared sink, giving audit consumers the full causal chain from match
to load decision.

The reactive `rustos_devmgr::run` loop re-matches the whole tree snapshot on
every generation advance (§18.4), but a node's decision is logged only the
**first** time it is reached and again only when it *changes* (e.g. `13002`
`NODE_UNBOUND` → `13001` `NODE_BOUND` once the late-bound catalogue arrives):
the loop carries a per-node `ReportedNodes`/`NodeReport` memory, so
re-evaluating a settled tree emits no record and the diagnostic log is never
re-flooded with identical lines (`AGENTS.md` §20 / §2.16). An unbound node is
thus *logged*, not re-logged.

The routine `NODE_UNBOUND` decision is additionally logged at **`Debug`**
(unlike the `Info` `NODE_BOUND` / `Warn` tie / load-failure decisions), so on
a default-`Info` boot the unbound nodes are dropped in O(1) by the level
filter *before* any `log_emit` syscall and never reach the serial line at
all — even on the first pass. On a real Raspberry Pi the firmware device tree
has ~120 nodes, almost all driverless; emitting one `Info` line each over the
flow-blocked debug UART (~116 ms/line) once delayed the `Root filesystem
passphrase:` prompt by tens of seconds by starving the keyboard report pump. The unbound
fact is still logged with its stable id when diagnostics are enabled (lower
the threshold), satisfying §18.4 without the boot-time flood (§20 / §2.16).

## Match-key emission

On aarch64 the match keys arrive through the **generic** hardware-tree
walk in `kernel/arch/aarch64::platform`: every `compatible`-carrying
device-tree node is emitted with its compatible strings as match keys
(devicetree most-specific-first order) and its translated `reg` /
`interrupts` as resource requests — no per-device recognition list
exists to grow (see [aarch64 platform
discovery](../platform/aarch64.md#platform-discovery-hardware-tree)).

## Remaining Stage 4.HW work

The Pi-4 USB-keyboard chain is now **entirely user space** (`plans/PI.md`
P10 D5d): the boot walk seeds the discovered `brcm,bcm2711-pcie` root
complex and VideoCore mailbox nodes, and `devmgr` autoloads the signed
`/System/Drivers/` bundles against them — the PCIe root-complex driver
binds the bridge and emits the VL805 PCI function, the VL805 driver reloads
the controller firmware over the mailbox and emits the `usb,xhci` node, and
the keyboard driver binds that and pumps key edges. Nothing of the chain is
compiled into the kernel: the in-kernel driver-candidate catalogue
(`kernel/rustos-kernel::driver_catalog`) is now the storage **bootstrap
floor only** — virtio-blk + EMMC2, the block drivers that must be up before
the signed store is reachable (§18.6). The remaining work, tracked in
`PLAN.md`, is the hotplug **removal** runtime path: the kernel side landed
(the `hw_remove_node` syscall, the mirror of `hw_emit_node` — a bus driver
retires a node it published and its subtree, ownership-checked and
fail-closed), and the producer/reactor — a bus driver's port-watcher that
calls it on a hot-remove, and the `devmgr` reaction that unloads the bound
driver — is Design D D4.
