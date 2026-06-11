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

A bind-table entry matches a node when its `HwMatchKey` equals one of
the node's match keys — same kind, and for `Compatible` keys the same
string, for numeric keys the same vendor/product/class triple. The
resolution across drivers is deterministic (`AGENTS.md` §18.3):

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
non-root node as above, and loads each winning driver exactly once — a
driver matched by several nodes serves them all through one load. A
load refusal fails only its node, closed (`AGENTS.md` §5.4); the walk
continues so one bad image cannot block boot.

### The `DriverLoader` seam

The §17.4 layering keeps `rustos-devmgr` on `lib/*` only, so the load
*mechanism* sits behind the `DriverLoader` trait:

```rust
pub trait DriverLoader {
    fn load(&mut self, path: &str, caller_caps: &CapabilitySet)
        -> Result<DriverHandle, Errno>;
}
```

The deployment's integration point implements it over the
[driver host](./host.md)'s `Host::load` pipeline (mapping `HostError`
via `as_errno`), so every load still passes the full §8 gate —
signature verification, `CAP_DRV_LOAD` / `CAP_DRV_KERNEL` checks, and
the spawner hand-off. The device manager never inspects or bypasses
those checks, and it never re-parses image bytes: candidate bind
tables arrive already decoded (fail-closed) by the gate's own
`ParsedImage::decode_bind_table`.

## Audit surface

Every match, load, skip, and failure is logged through `lib/log` with
a stable event id in the device manager's reserved `13000..14000`
range (`rustos_devmgr::events`):

| Event id | Meaning |
|----------|---------|
| `13001` `NODE_BOUND` | a node was bound; fields: `node`, `path`, `handle` |
| `13002` `NODE_UNBOUND` | no driver matched; never an error (§18.4) |
| `13003` `NODE_TIE_REJECTED` | unbroken highest-priority tie refused; field: `priority` |
| `13004` `NODE_LOAD_FAILED` | the load gate refused the winner; fields: `path`, `errno` |

The drvhost gate's own `7000`-range records interleave with these on a
shared sink, giving audit consumers the full causal chain from match
to load decision.

## Remaining Stage 4.HW work

Generic match-key emission (replacing the hand-grown list of node
types `kernel/arch/aarch64/src/fdt.rs` recognises) and the
hotplug/removal runtime path are tracked in `PLAN.md` Stage 4.HW.
