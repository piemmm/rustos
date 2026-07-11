# DEVICES.md — Device inventory commands and USB mass storage

This is the staged build plan for RustOS's next tier of device support:

- **DEVICE1** — `lspci` and `lsusb` system command apps that list the
  discovered PCI/PCIe and USB devices with human-readable vendor/device
  names resolved from vetted, locally cached snapshots of the public PCI
  and USB ID databases.
- **DEVICE2** — a USB mass-storage class driver and the hotplug automount
  path: a plugged-in USB disk's filesystems appear under the `Storage:`
  catalog with deterministic, collision-free names and sane permissions;
  surprise removal is handled with retained uncommitted data, a syslog
  record, and an explicit force-unmount / verified re-insert recovery
  choice.

`AGENTS.md` is binding — read it and `PLAN.md` first. Every rule in this
file is binding too. Related plans: `plans/USB.md` (the modular USB stack
this builds on), `docs/src/filesystem/drives.md` + `plans/DRIVES.md` /
`plans/ALIAS.md` (the storage namespace the automount publishes into),
`plans/APPS.md` (command-app packaging), `plans/SYSLOG.md` (event
logging), `plans/CAPABILITY_USE.md` (capability lifecycle).

---

## 0. Starting point (what already exists)

Facts the stages below build on, so no stage re-derives them:

- **The hardware tree is the single device inventory (§18.1).** Every
  discovered PCI function and USB interface is a `HwNode` whose
  `HwMatchKey`s already carry the numeric identities the listing commands
  need: PCI `vendor:device:class` and USB `vid:pid:class` triples
  (`lib/abi/src/hwtree.rs`). The tree is exposed read-only through the
  `CAP_SYSINFO_HW`-gated `hw_tree_read`/`hw_tree_wait` syscalls and
  through `sysinfod`'s `HARDWARE_TREE` query, which the existing
  `sysinfo hardware` command already renders. No new kernel surface is
  needed to *list* devices — only to name them.
- **The modular USB stack is complete** (`plans/USB.md` U1–U5): bus
  driver → user-space xHCI HCD owning one controller → per-interface
  nodes → class drivers over the URB transport IPC
  (`lib/abi/src/usb_urb.rs`), with event-driven hotplug in both
  directions and the kernel driver-unload mechanism. The URB transport
  serves control-IN, the no-data control-OUT (since D2 — a SETUP-only
  class request; a control-OUT *data stage* stays refused, it has no
  consumer), interrupt, and — since D1 (§2.2) — bulk transfers.
  **The engine and HCD serve every reachable device concurrently**
  (`UsbDevice::bring_up` walks every connected hub port into a table of
  up to `MAX_DEVICES` devices, each with its own layout region and its
  own HCD transport — endpoint, shared buffer, interface node): a
  keyboard and a storage stick plugged in together are both served,
  fixing the Pi 4 boot defect where a plugged-in stick won the engine's
  single device slot and the keyboard never enumerated. Hot-plug
  attaches/detaches exactly the affected device's index; a failed
  per-port enumeration is skipped with its slot released
  (`plans/USB.md` §1.1).
- **The block-driver contract exists**
  (`rustos_abi::driver::block::Block`), implemented by
  `drivers/storage/virtio_blk` and `drivers/storage/emmc2` with bounded,
  fixed-cost DMA staging. Filesystem crates exist for ext4, FAT32, and
  RustFS; `lib/partition` is the shared MBR/GPT layer.
- **The storage namespace is specified and partially landed.** The
  binding spec (`docs/src/filesystem/drives.md`) defines the forest of
  named roots: canonical `id::<volume-id>` identity, aliases, the
  `Storage:` catalog as a synthetic view of published non-core roots, and
  the hotplug lifecycle (tree update → driver stack attach → `id::` root
  → alias policy → catalog → sysinfo visibility). The four machine
  aliases and the `fs_*` descriptor ABI are landed; the durable
  `id::`/`fs::` resolver roots and the volume forest are still open
  (PLAN.md P4) and are a prerequisite DEVICE2 completes, not assumes.
- **Command-app conventions are fixed** (`plans/APPS.md`, §16.5/§16.7):
  a command is a full self-contained bundle in the system app store with
  its `Help/` tree, follows the established GNU/`pciutils`/`usbutils`
  option and output surface, emits additive `stdinfo` records, and
  acquires authority only through its manifest capability request.
- **Asset-generation precedent**: `cargo xtask font-atlas` generates
  checked-in Rust data from a committed, licence-vetted asset in
  `lib/font/assets/` and verifies drift in CI. The ID-database pipeline
  follows the same shape.

---

## 1. Stage DEVICE1 — `lspci` and `lsusb`

### 1.1 The ID databases: import, vetting, and cache

The public databases are:

- PCI: `https://pci-ids.ucw.cz/v2.2/pci.ids` (~1.6 MiB plain text,
  versioned and dated in its header, explicitly dual-licensed
  GPL-2.0-or-later / BSD-3-Clause — both already on the `deny.toml`
  allow-list).
- USB: `http://www.linux-usb.org/usb.ids` (~0.7 MiB, same tab-indented
  line grammar, distributed with `usbutils` under GPL-2.0-or-later; no
  in-file licence header, so the import records the upstream licence
  statement in the snapshot provenance header and the refresh stops for
  human review if upstream terms change, §15.7). Upstream publishes **no
  valid TLS endpoint** for this file (`linux-usb.org`, `www.linux-usb.org`,
  and the maintainer mirror all serve broken certificates), so the fetch
  uses the canonical HTTP URL and integrity rests on the recorded SHA-256
  plus the human review of the snapshot diff — CI and builds never fetch.

Both share one grammar: `vendor_id  name` lines, single-tab
`device_id  name` children, two-tab subsystem children (pci.ids only), plus
trailing tagged class/other tables (`C`, and for usb.ids `AT`, `HID`, `R`,
`BIAS`, `PHY`, `HUT`, `L`, `HCC`, `VT` — a closed set; an unknown tag stops
the import for review). One parser handles both (§2.2).

**Pipeline (the `cargo xtask devids` subcommand):**

1. `cargo xtask devids --fetch` — developer-run only, never CI or the
   build (§19.3 forbids build/post-install network fetches; builds stay
   offline and reproducible). It downloads both files over HTTPS where
   offered (pci.ids; usb.ids has no valid TLS endpoint, see above),
   runs the vetting filter (below), and rewrites the committed snapshots
   under `lib/devids/assets/` with a provenance header: upstream URL,
   upstream version/date lines, fetch date, SHA-256 of the raw download,
   and the licence statement. The refresh diff is human-reviewed like any
   other change (§2.6); CI never sees the network.
2. `cargo xtask devids --write` — regenerates the compact lookup tables
   (see §1.2) from the committed snapshots.
3. `cargo xtask devids` (no flag, part of `cargo xtask ci`) — re-runs the
   converter over the committed snapshots and fails closed on any drift
   between snapshot and generated tables, exactly like `c-header` and
   `font-atlas`.

**Vetting filter (the "checked for malicious content" gate).** The raw
download is untrusted input (§19.5) and its strings end up on users'
terminals, so the filter is strict, fail-closed, and bounded (§24.4):

- The file must parse under the exact line grammar above; any line that
  is not a comment, a blank, or a well-formed entry rejects the whole
  import (never skip-and-continue on structure, so a smuggled section
  cannot hide).
- Every name must be valid UTF-8 and contain **no control bytes** other
  than the structural tab/newline — no ESC/CSI/C0/C1, so a hostile entry
  cannot inject terminal escape sequences through `lspci` output. Names
  are length-bounded; ids must be exact-width lowercase hex; duplicate
  ids within a scope are rejected.
- Entry counts and total size are bounded (fixed security bounds sized
  with generous headroom over today's ~40k entries).
- The fetch verifies the transport (TLS for pci.ids) and records the
  SHA-256 so the reviewed snapshot is pinned; a later `--write`/CI run
  operates only on the pinned, reviewed bytes (§19.3 spirit).

The vetting logic lives in `lib/devids` (host-testable, shared by the
xtask generator — one definition, §2.2) and carries a fuzz harness over
the raw-text parser (§19.6): it consumes genuinely untrusted bytes.

### 1.2 `lib/devids` — the lookup engine

A new `lib/*` crate (`rustos-devids`, `no_std`+`alloc`-free lookup;
adding it updates `AGENTS.md` §3 and `PLAN.md` in the same change, §6):

- **Compact table format.** The generator emits, per database, a sorted
  binary table (vendor records → device records → subsystem/interface
  records, plus the class tables) with an index the lookup binary-searches
  — O(log n), no allocation, no whole-file scan (§2.16). The format is
  self-identifying (magic, version, counts, bounds) and the runtime
  decoder validates every offset/length fail-closed (§5.4) — the file
  ships on the read-only system volume but is still parsed as data, never
  trusted blindly.
- **API.** `pci_vendor(u16) -> Option<&str>`,
  `pci_device(u16, u16) -> Option<&str>`,
  `pci_class(u32) -> Option<(&str, &str, Option<&str>)>` (class /
  subclass / prog-if), and the `usb_*` equivalents (vendor, product,
  class triple, plus the usb.ids auxiliary tables only if a consumer
  renders them — no speculative surface, §2.3). An unknown id is `None`;
  the caller renders the numeric form (fail closed, never fabricate).
- **Data placement.** Each command bundle carries its own database file
  as a bundle resource (§16.5 self-containment): `lspci.app/Resources/
  pci.ids.bin` and `lsusb.app/Resources/usb.ids.bin`, laid onto the
  image from the generated tables by the image builder through the same
  discovered-from-disk path as `Help/` trees — never `include_bytes!`
  into a binary (help/data-on-volume rule, §16.5) and never a second
  copy of the tables in the tree. `lib/devids` is the one source of the
  encode + decode + lookup; the two `.bin` files are generated artefacts.

### 1.3 The `lspci` command app (`userland/apps/lspci`)

- **Data source:** the `sysinfod` `HARDWARE_TREE` query through the
  existing `lib/procinfo` client seam — never a kernel bypass, never a
  `/proc` fabrication (§16.6). The manifest requests `CAP_SYSINFO_HW`;
  a refusal defeats the command's whole purpose, so it exits with the
  reason on stderr (§2.24), never a fabricated empty list.
- **Selection:** nodes whose match keys are `HwMatchKind::Pci`, rendered
  in stable bus order (parent-chain order from the tree).
- **Output follows `pciutils` (§16.7 principle):** default one line per
  function — address, class name, vendor + device name (numeric id when
  the database has no entry, matching `lspci`'s `Device <id>` form) —
  with the established options implemented over what the model carries:
  `-n` (numeric), `-nn` (names + ids), `-d <vendor>:<device>` and
  `-s <slot>` filters, `-v` (the node's resources: MMIO windows, IRQ
  lines — the grant *requests* the tree records, no secrets), `-t` (tree
  view). Where RustOS's model genuinely lacks a field Linux has the
  option is withheld or degrades honestly rather than fabricating:
  subsystem ids are not recorded by the hardware tree today (extending
  `hwtree` is a deliberate carve-out for a later ABI revision, not
  smuggled in here, §18.1), and `-k` (bound driver bundle) is not
  offered until the system publishes driver-binding records through the
  store/sysinfo path — a clear unknown-option error beats a flag that
  cannot be served honestly.
- **`stdinfo`:** additive records only (§20.1) — e.g. an `omission`
  record when unnamed devices were rendered numerically, with the
  unnamed count and whether the database loaded in `ai` context (the
  compiled table deliberately carries no upstream version string).
- Full `Help/` tree (en-US canonical), rustdoc, and a
  `docs/src/userland/` page in the same change (§13, §16.5).

### 1.4 The `lsusb` command app (`userland/apps/lsusb`)

- Same data source, capability posture, and bundle shape as `lspci`.
- **Selection:** nodes with `HwMatchKind::Usb` keys (the per-interface
  nodes the HCD emits) plus their parent controller nodes for the `-t`
  topology view.
- **Output follows `usbutils`:** default `Bus NNN Device NNN: ID
  vvvv:pppp Vendor Product` lines — bus/device numbers derive from the
  tree's stable node ids (a deliberate, documented divergence: RustOS
  numbers come from the hardware tree, not a Linux devnum, §16.7
  divergence-by-concept), `-v` (interface class/subclass/protocol names
  from the usb.ids class table), `-s`/`-d` filters, `-t` (controller →
  interface tree).
- Same `stdinfo`, `Help/`, and docs obligations as `lspci`.

### 1.5 DEVICE1 increments

Each increment ends green on the whole-project gate (§7).

- **V1 — `lib/devids` + `cargo xtask devids`. Done.** `lib/devids` holds
  the shared parser + vetting filter + compact-table encoder and the
  alloc-free binary-search decoder (`DevIds`: `vendor`/`device`/`class`/
  `subclass`/`prog_if`); the xtask subcommand (`--fetch` / `--write` /
  verify) imports with full provenance headers and drift-checks as a
  `cargo xtask ci` static gate; the vetted snapshots live in
  `lib/devids/assets/`, each generated table in its consuming bundle's
  `Resources/`; the `fuzz_devids` harness covers both untrusted surfaces.
  Reality-driven decisions: usb.ids is fetched over upstream's canonical
  HTTP URL (no valid TLS offered) with SHA-256 + review as integrity; a
  stray ISO-8859-1 byte in usb.ids is deterministically promoted to
  UTF-8 and recorded in the provenance header; auxiliary-section ids
  accept 1–4 hex digits of either case (upstream publishes uppercase HUT
  usages) while every emitted scope stays exact-width lowercase; PCI
  subsystem entries and the auxiliary tables are validated but not
  encoded (no consumer renders them). §3/PLAN.md carry the crate and
  subcommand.
- **V2 — `lspci`. Done.** `userland/apps/lspci`: the pure engine
  (fail-closed `HwNode` decode, stable parent-chain bus order, the
  `-n`/`-nn`/`-v`/`-t`/`-d`/`-s` surface, numeric fallbacks, the
  `pci.names_unresolved` fd-3 advisory) over the shared
  `rustos_procinfo::call`/`Output`/`HelpSource` seams; the freestanding
  `Run` loads `Resources/pci.ids.bin` through the VFS and degrades to
  numeric ids (reason on stderr) on a missing/invalid table, and treats a
  refused `CAP_SYSINFO_HW` as fatal with its reason. Thirteen-locale
  `Help/` with the switch-pinning test; host unit tests run over a canned
  tree + a fixture database compiled through the real `lib/devids`
  pipeline. Reality-driven decisions: a function's address is its stable
  hardware-tree node id (`#<node>`, the lsusb-style documented
  divergence — the tree records no BDF); `-k` is withheld (no
  driver-binding records exist to serve it honestly, §1.3); the compiled
  pci table moved into the bundle (`userland/apps/lspci/Resources/`, one
  tree copy, `cargo xtask devids` retargeted). Enabling
  infrastructure landed with it: generic bundle-`Resources/` discovery
  (`rustos_syshelp::RESOURCE_FILES` — build-discovered from each crate's
  on-disk `Resources/`, never a per-bundle list) feeding the signed
  `AppInfo` content digest and both planters (`tools/mkimage`, the QEMU
  encrypted-root fixture); the kernel load gate already re-hashes the
  whole bundle, so a tampered resource refuses the spawn. QEMU coverage:
  the SP10b pipeline vertical spawns `lspci --help` from the planted
  store, proving the resource-carrying bundle through the content-hash
  gate end to end; the emulated aarch64 `virt` image drives virtio-mmio
  devices and publishes no PCI-function nodes yet, so the full listing is
  host-proven and a live-listing vertical rides the first emulated
  PCI-function target that carries the app store.
- **V3 — `lsusb`. Done.** `userland/apps/lsusb`, as V2 for the USB view:
  the pure engine (interface selection over `HwMatchKind::Usb` keys, the
  `usbutils` default `Bus NNN Device NNN: ID vvvv:pppp <vendor>
  <product>` line, `-v` interface class/subclass/protocol names from the
  usb.ids class tables — decimal descriptor values, as `usbutils` prints
  them — `-t` controller→interface topology, `-d [<vendor>]:[<product>]`
  and `-s [[<bus>]:][<devnum>]` filters, the `usb.names_unresolved` fd-3
  advisory) over the same seams as `lspci`; the freestanding `Run` loads
  `Resources/usb.ids.bin` and degrades to bare ids (reason on stderr) on
  a missing/invalid table; thirteen-locale `Help/` with the
  switch-pinning test; host unit tests over a canned tree + a fixture
  database compiled through the real `lib/devids` pipeline.
  Reality-driven decisions: a device's bus number is its controller's
  stable hardware-tree node id and its device number its own node id
  (RustOS has no Linux devnum registry — the §1.4 documented
  divergence), and one line renders per *interface* node (the inventory
  the HCD emits), with no root-hub pseudo-devices; an identity the
  database does not name shows only its `ID vvvv:pppp` (the `usbutils`
  omission shape), counted on fd 3. The shared hardware-tree walk
  (fail-closed decode, stable bus order, depth, ancestor-keep, class
  labels) was hoisted into `rustos_procinfo::hwtree` and `lspci`
  refitted onto it, so the two listing tools render through one
  definition; `cargo xtask devids` now writes `usb.ids.bin` into the
  bundle and the `lib/devids/tables/` staging home is deleted. QEMU
  coverage follows the V2 precedent: the SP10b pipeline vertical spawns
  `lsusb --help` from the planted store (the resource-carrying bundle
  through the content-hash gate); no emulated fixture publishes
  USB-interface nodes yet, so the live listing is host-proven and a
  live-listing vertical rides the first emulated USB target that
  carries the app store (Pi 4 metal acceptance otherwise).

---

## 2. Stage DEVICE2 — USB mass storage and hotplug automount

### 2.1 Target shape

```
xHCI HCD (drivers/bus/usb/xhci)
  └─ emits usb-interface node 08:06:50 (mass storage, SCSI, bulk-only)
       └─ drivers/storage/usb_msd  (class driver: BOT + SCSI over URBs)
            └─ emits one storage-class block node per LUN
                 └─ volume manager: partition probe → fs probe →
                    id:: root published → alias + Storage:/ catalog →
                    users read/write per mount policy
```

Every edge is discovery + match + public ABI (§2.20, §17.4): the class
driver knows no controller, the volume layer knows no bus, and nothing
names a sibling crate.

### 2.2 D1 — bulk transfers on the URB transport. **Done.**

- `lib/usb` serves real bulk transfers: a mass-storage interface's bulk
  endpoint pair (the first bulk-IN + bulk-OUT the configuration
  descriptor reports, never assumed) is configured at enumeration with
  per-direction transfer rings (`BULK_RING_TRBS` = 9) whose data slots
  each own a 4 KiB staging buffer, so several bulk TDs queue per
  direction (`queue_bulk_in`/`queue_bulk_out`, ring-full → `Busy`) and
  complete in order through the non-blocking `poll_bulk`; short packets
  report the honest byte count from the residual. A device STALL is
  recovered in place — Reset Endpoint → ring rebuild + Set TR Dequeue
  Pointer → `CLEAR_FEATURE(ENDPOINT_HALT)` on the **device's own** EP0 —
  with every abandoned TD answered, surfaced as the new distinct
  `Errno::EndpointStalled` (29) / `DriverError::EndpointStalled` (15) so
  the D2 BOT driver can run its own recovery (C headers regenerated;
  the generator's DriverError table also gained the previously-omitted
  `SEAT_REVOKED`).
- `UrbEngine` gained `bulk_in`/`bulk_out` (arm-then-reap, `Ok(None)`
  while in flight), `drive_urb` validates bulk fail-closed (never
  endpoint 0; the engine owns the endpoint map), and
  `UrbClient::{bulk_in, bulk_out}` are the class-side builders. The HCD
  stays the single wait-set event loop: `UrbService` holds a bulk URB
  outstanding and completes it on the controller event exactly like an
  interrupt URB; the class driver still holds **zero** DMA authority
  (the HCD bounce-copies through its staging buffers).
- Reality-driven fixes landed with it: `restore_hub_active` now **parks**
  a downstream device's EP0 ring (`device_ep0_ring`) instead of dropping
  it, so a post-enumeration device-targeted control transfer (a URB
  control-IN, the recovery's `CLEAR_FEATURE`) switches to the device via
  `activate_device_control` and never wrongly targets the hub — the Pi
  topology, where every stick sits behind the integrated hub;
  `describe_device` derives the emitted node's class from the interface
  class byte (`0x08` → `Storage`, `0x03` → `Input`, else `Other`)
  instead of hardcoding `Input`. Teardown/reset paths clear all bulk
  state.
- Host-proven over the register-level mock, which grew a scripted
  mass-storage device (fixture endpoints deliberately EP3-IN/EP4-OUT →
  DCIs 7/8, two-endpoint Configure Endpoint, per-direction halt state
  machine that serves nothing until Reset Endpoint → Set TR Dequeue →
  device-side `CLEAR_FEATURE` run **in order**, and a hub-mistargeted
  clear STALLs EP0 loudly): enumeration/node emission, OUT/IN
  round-trips, short packets, in-order multi-TD completion + queue-depth
  bound, stall recovery answering every queued TD (direct and
  downstream-of-hub), and URB-seam validation; plus transport-seam and
  HCD `UrbService` bulk tests (held-then-completed, in-band
  `EndpointStalled`).

### 2.3 D2 — `drivers/storage/usb_msd` (Bulk-Only Transport class driver). **Done.**

- `drivers/storage/usb_msd/` (storage-class leaf, vendor-neutral
  namespace, §8): a pure user-space class driver binding
  `HwMatchKey::usb(0, 0, 0x08_06_50)`. The crate is a host-testable `lib`
  (`desc` — fail-closed configuration-descriptor reader; `bot` — the
  BOT/SCSI engine over the `MsdTransport` seam; `serve` — the
  block-service state machine) plus the freestanding `Run` program. Caps:
  `CAP_SHM`, `CAP_IPC_ENDPOINT`, `CAP_IPC_BIND_PRIVILEGED` (the per-LUN
  serve endpoints), `CAP_HW_EMIT`, `CAP_LOG_EMIT`; no MMIO/DMA/IRQ.
- BOT + the full planned SCSI subset are implemented with every
  device-supplied field bounds-checked fail-closed: CSW
  signature/tag/residue/status validation, data-phase-stall → CSW
  fall-through, CSW-stall single retry, tag-mismatch / corrupt-CSW /
  phase-error → Bulk-Only Mass Storage Reset; `GET MAX LUN` (STALL means
  one LUN, >15 fails closed), `INQUIRY`, bounded `TEST UNIT
  READY`/`REQUEST SENSE` ready drain, `READ CAPACITY(10)`/`(16)`
  (validated power-of-two 512–4096 block size; 16-byte form past the
  32-bit LBA horizon), `READ`/`WRITE(10)`/`(16)` selected by range,
  `SYNCHRONIZE CACHE(10)` (ILLEGAL REQUEST = no cache = success),
  `MODE SENSE(6)` WP bit (a refused MODE SENSE reports write-enabled —
  the established meaning — while enforcement stays driver-side:
  `LunBlock` refuses writes to protected media before any byte reaches
  the device). Each LUN is a `rustos_abi::driver::block::Block`
  (`LunBlock`, chunked through the fixed 32 KiB window so per-device cost
  never scales with request length) plus a `Flush` seam (the `Block`
  trait carries no flush).
- **Reality-driven decisions.** (1) No block-over-IPC protocol existed
  (the existing block drivers are in-kernel bootstrap floor), so D2
  defined it: `lib/abi/src/blkio.rs` — fixed-frame `BlkRequest`
  (geometry/read/write/flush) + `BlkCompletion` (geometry + read-only
  flag, `-errno` status word) over a `BLK_DATA_LEN` = 32 KiB shared data
  window, the same call-endpoint + shared-window IPC shape as the URB
  transport; covered by the `fuzz_decode` harness. The `Run` binds one
  endpoint + window per ready LUN and emits one Storage-class node
  carrying them (compatible key `rustos,usb-msd-lun` — the D3 volume
  manager's selector), served on a wait-set; detach retracts the nodes
  and exits 0 for reload. (2) The emitted interface node carries no
  endpoint numbers and the bulk server validates them, so the driver
  reads the device's own configuration descriptor (bounded, fail-closed)
  to derive the interface number and bulk pair — the same facts the HCD
  derived, never assumed. (3) The planned no-data control-OUT landed with
  its first consumer: `UrbEngine::control_no_data` / `drive_urb`
  Control+Out+len==0 / `UrbClient::control_no_data` (a data-stage OUT
  stays refused); the engine reuses the existing SETUP_TRT_NO_DATA
  control path. (4) The HCD's per-interface URB buffer grew from 64 B to
  one bulk chunk (`BULK_BUF_LEN` = 4096, the same page) so bulk data fits;
  a class-side transfer splits into ≤4 KiB URB chunks.
- Host-proven over scripted doubles: BOT framing, tag mismatch,
  reset-recovery trails, CSW-stall retry, stalled data phases, short
  reads refused, sense mapping (DATA PROTECT → `PermissionDenied`),
  capacity validation incl. a 100 TB-class unit, write-protect
  enforcement before the wire, chunk sequencing, sensitive-window scrub,
  multi-LUN CBW addressing; hostile descriptor streams; the whole
  blkio request surface over an in-memory device. No QEMU fixture
  publishes USB interface nodes (QEMU models no Pi USB — the U4/V3
  precedent), so the live path is Pi 4 metal acceptance and a
  `qemu-xhci` + `usb-storage` vertical rides the first emulated target
  that carries the USB stack. The bundle ships in the Pi image
  (`Drivers/storage/usb_msd/Run`, signed, least-privilege manifest).

### 2.4 D3 — the volume forest and automount

This lands the still-open volume forest (PLAN.md P4) as its centre; the
work is completed here, not stubbed around (§2.19). It is staged as three
sub-increments, each green alone on the whole-project gate (§7): the
durable-identity core first, then runtime multi-root attach, then the
volume-policy service.

#### 2.4.1 D3a — durable `id::` roots for mounted volumes. **Done.**

- `lib/path` gained `Root::VolumeId` **in place** (§2.13 — no second
  parser): `id::<volume-id>/path` parses only from the canonical
  hyphenated lowercase UUID spelling into a typed 16-byte `VolumeId`
  (any other spelling is `VolumeIdInvalid`, fail-closed; `..` cannot
  escape the root; `Display` renders the canonical spelling and
  re-parses to the same value, covered by the `fuzz_path` round-trip
  harness, whose templates already mutate an `id::` form).
- The **kernel volume forest** (`kernel/core::fs::volumes::VolumeForest`)
  is the registry from a volume's stable identity to the live root:
  threaded `BootInfo::with_volumes` → dispatch hook → syscall handlers
  exactly like the other late-installed seams (fail-closed
  `NULL_VOLUME_FOREST` default), and read by the single kernel
  path-resolution entry point (`resolve_against_cwd`), so an
  `id::`-rooted path resolves to the `/`-view location the published
  volume's root backs and is then authorised by the secured VFS exactly
  as the equivalent view path — never a policy bypass. A nil or
  duplicate identity is refused at publish; an unpublished identity
  fails closed `NotFound`.
- `RustFs::volume_uuid()` exposes the per-volume UUID (already minted at
  format and verified into every block header), and the boot mount paths
  publish both boot volumes — the read-only System volume at the
  `System` view prefix, the encrypted writable root at the view root —
  with the audited `fs.root.publish.{allow,deny}` events (4170/4171,
  drives.md §23). `docs/src/filesystem/drives.md` §12/§21 and
  `docs/src/lib/path.md` record the landed state.
- **Deliberate scope decisions.** Volume identity crosses the ABI only
  as path *text* today, so no `lib/abi` volume-id type is minted ahead
  of its first typed consumer (the D3c storage sysinfo queries,
  §2.3/§2.4). Forest unpublication landed with its first producer, the
  D3b runtime detach; the boot volumes themselves are published once and
  never withdrawn.

#### 2.4.2 D3b — runtime volume attach and multi-root publication. **Done.**

- **The ABI**: `volume_attach` (78) / `volume_detach` (79), both
  `CAP_FS_MOUNT`-gated and audited, taking the fixed-frame
  `lib/abi/src/volume.rs` requests (`VolumeAttachRequest`: endpoint +
  window + probed partition extent + fstype + validated catalog name;
  `VolumeDetachRequest`: the 16-byte volume identity). The attach handler
  additionally requires the caller's own kernel-minted resource grants to
  cover **both** transport resources the request names (the endpoint and
  the shared window forwarded on the matched storage node), so the mount
  authority alone can never reach another driver's transport. `lib/rt`
  wrappers and `ros_sys_*` stubs landed; C headers regenerated.
- **The kernel blkio client** (`kernel/core::fs::blkclient::BlkClient`):
  a `Block` over the per-LUN call endpoint + shared window, using the
  `ipc_call` post → wake-server → park → take-reply discipline (a
  destroyed endpoint cancels the in-flight call — typed fault, never a
  hang). The window is reached through a counted **kernel hold**
  (`sharedreg::kernel_hold` over the new `SharedMemFacility::
  kernel_window` direct-map translation), so the frames outlive the
  owning driver's exit while the kernel still reads them. Geometry is
  validated fail-closed at connect; transfers chunk through the 32 KiB
  window; the device write policy is enforced client-side too.
- **Runtime-mutable mount table**: `Vfs` now holds its `MountTable`
  behind an `RwLock` (guards held only for lookups, never across a
  park), `LateFilesystem` shares drivers by `Arc<SleepLock<_>>` and
  gained `unregister` (a detached volume's driver drops cleanly — no
  leak per unplug, in-flight operations finish on their own clones), and
  the forest gained `unpublish`. A runtime mount carries its own
  permission template (`MountTable::mount_with_template`) because its
  mount point has no node in the boot layout tree; ancestor search
  authorisation still walks the real tree.
- **The service** (`kernel/rustos-kernel::volume_service`, installed via
  the `BootInfo::with_volume_service` seam → `NULL_VOLUME_SERVICE`
  fail-closed default): windows the extent (`PartitionBlock`), opens the
  matched filesystem (RustFS / ext4 / FAT32), mounts under
  `/Storage/<name>` with `nosuid,nodev,noexec` (+`ro` per the device),
  and publishes the identity **last** with full unwind on any refusal.
  Detach orders fs-flush → device-flush → unmount → unregister →
  unpublish and fails closed (volume stays attached) on a flush error; a
  vanished endpoint (device already gone) is not a refusal. Audit:
  4172/4173 (`fs.hotplug.root_added` allow/deny), 4174/4175
  (`root_removed`), publication through the one shared
  `fs.root.publish.{allow,deny}` definition (4170/4171).
- **Reality-driven decisions.** (1) ext4/FAT32 lacked the
  `FilesystemStats` surface and any volume identity: both gained honest
  impls (ext4: live superblock counts + `s_uuid`; FAT32: an open-time
  FAT scan maintained by the allocator, serial‖label‖tag identity —
  content-derived, as drives.md §8 sanctions for formats without a
  UUID), and FAT32 gained the uniform restrictive `FilesystemSecurity`
  posture (stores refused — silently-lossy records are forbidden). The
  ext4 **formatter wrote a nil `s_uuid`** — a real defect the new tests
  exposed; `Ext4::format` now takes the caller-minted UUID and refuses
  nil. (2) A RustFS attach uses the well-known key (non-secret volumes,
  as the System volume): a privately-keyed volume refuses with a typed
  error, and key-provisioned attach arrives with volmgr's key policy —
  the kernel never guesses a secret. (3) The mount template is the
  restrictive system-owned default until D3c's storage-group identity
  map lands; listing `/Storage` (the catalog *view*) is D3c's synthetic
  catalog work — D3b delivers resolution, not enumeration.
- Host-proven end to end: the lifecycle test serves a formatted FAT32
  image over a genuine call endpoint from another thread, attaches it,
  resolves the published root, reads the file through the production
  `fs_*` service, refuses a duplicate-identity attach with clean unwind,
  detaches (device flush observed), and fails closed on re-detach — plus
  unit tests across blkclient (chunking, hostile geometry, stall/detach
  faults), sharedreg kernel holds, mount templates, forest unpublish,
  and both filesystems' new stats/identity surfaces.

#### 2.4.3 D3c — `volmgr`: the automount policy driver. **Done.**

- **`drivers/storage/volmgr`** owns automount policy, as `devmgr` owns
  driver policy — but as a **per-node autoloaded policy driver**, not a
  singleton tree-watching service. Reality-driven design decision: the
  landed D3b security model gates the blkio call endpoint behind the
  kernel-minted per-endpoint grant (`ipc_call` refuses an ungranted
  caller before any byte moves) and `volume_attach` additionally
  requires the caller's grants to cover both transport resources; grants
  are minted in exactly one place, the per-node driver-admission spawn.
  A singleton watcher could therefore neither probe nor attach without
  new kernel surface, whereas the existing discovery/match/grant
  machinery gives the per-node instance exactly one device's transport
  authority — least privilege, zero new kernel surface. The crate's
  `BIND_KEYS` selects the block-service node's compatible key
  (`rustos,usb-msd-lun` today; a future hot-pluggable block source adds
  its key as data — the engine names no bus, §2.20).
- **The instance** (caps: `CAP_SHM`, `CAP_IPC_ENDPOINT`, `CAP_FS_MOUNT`,
  `CAP_LOG_EMIT`; no MMIO/DMA/IRQ/emit): connects a **read-only** blkio
  `Block` client (hostile geometry refused at connect; `write_blocks`
  refuses by construction), probes a whole-device filesystem signature
  first (superfloppy), else the GPT/MBR table (`lib/partition`), probing
  each present partition's head **by content** through the new
  `lib/fsprobe` crate (the one home of the RustFS/ext4/FAT32 signature,
  label, and identity definitions — the fs drivers import their magic/
  identity from it, so probe and driver can never disagree; §2.2), and
  asks the kernel to attach each recognised volume (the D3b
  `volume_attach` path: kernel re-validates grants/extent/name, opens
  the filesystem, mounts under `/Storage/<name>`, publishes the durable
  `id::` root). It then exits `0` — run-to-completion; the kernel-held
  mount outlives it, and a re-plug re-discovers and reloads afresh.
  Events 4180–4184 log probe/attach/nothing-attachable/device-failed.
- **Naming is deterministic, never a coin-flip:**
  1. the volume's own label, sanitised through the alias character rules
     (ALIAS.md §5.2: lowercased `a-z0-9-_`, everything else dropped,
     leading separators stripped, empty falls through);
  2. else `<fstype><n>` (`fat1`, `ext1`, `rustfs1`), `n` the 1-based
     per-type ordinal in device order;
  3. a name the kernel reports in use (`AlreadyExists`) gets the
     volume-identity fingerprint appended (ALIAS.md §3.8, rendered by
     `rustos_fsprobe::fingerprint` — lowercase Crockford base32, spelled
     with `-` because the volume-name grammar has no `@`), lengthened
     4 → 8 → full per retry; distinct identities have distinct full
     fingerprints, so the sequence terminates. Re-inserting the same
     volume re-derives the same name (stable identity), so a user's
     scripts keep working.
- **Defect fixed en route (§2.18):** `lib/partition`'s `mbr::encode`
  silently *dropped* a `PartitionType::Other` partition (its type byte
  encoded as the unused marker). `type_byte_for` now returns `Option`
  and `encode` refuses an unrepresentable role
  (`MbrError::UnrepresentableRole`), with a regression test.
- Host-proven: the blkio client (hostile geometry, chunking, shape
  violations, refused writes, corrupt replies), the probe plan
  (superfloppy, content-over-type partitions, lying extents, blank
  device, device fault, per-type ordinals), the naming policy
  (sanitisation, fallback, candidate truncation/uniqueness), and
  `lib/fsprobe` (signatures, hostile heads, probe order, fingerprint
  bit-coverage). Live path: Pi 4 metal acceptance (QEMU models no Pi
  USB — the D2/U4 precedent); the bundle ships in the Pi image
  (`Drivers/storage/volmgr/Run`, signed, least-privilege manifest).
  The autoload gate's delegatable superset
  (`unlock_service::autoload_caps`) carries `CAP_FS_MOUNT` so the signed
  manifest is admissible through the store gate — the first metal boot
  exposed its absence as an `id=7006` capability-escalation refusal
  (the vcmailbox `CAP_IPC_BIND_PRIVILEGED` precedent); the per-driver
  manifest∩superset intersection still binds. Metal re-verification of
  the end-to-end automount is the outstanding live check.

#### 2.4.4 D3d — mount-policy permissions and the catalog view. **Done.**

- **The storage-group identity map** (§5.3, §16.3): the well-known
  `storage` group is defined once (`rustos_users::STORAGE_GROUP`, seed gid
  `STORAGE_GID` = 100) and resolved **by name** from the loaded
  `/System/Security/Groups` registry by the trusted root-unlock step
  (`UnlockInstall::storage_gid` → the set-once
  `rustos_kernel::volume_policy::LATE_STORAGE_GID` cell). A runtime-attached
  ownerless filesystem (FAT32) is wrapped in `GroupMappedFs`: every node
  reports system ownership under that group — directories `0o775`, files
  `0o664`, `set_security` refused (the format cannot hold a record) — and
  the mount template matches, so any logged-in member reads and writes the
  medium without ambient authority while non-members read only. No
  installed gid (or a registry without the group) leaves the volume
  restrictively system-owned (fail closed, never an invented gid); volumes
  with a real owner model (ext4, RustFS) are never wrapped. Removable
  mounts keep `nosuid,nodev,noexec` (landed in D3b); `CAP_FS_MOUNT_RELAX`
  relaxation stays future work with its own enforcement point.
- **Catalog enumeration:** listing a driver-backed directory merges the
  backed mounts sitting directly beneath it (`MountTable::direct_children`
  → the `fs_readdir` service), so `/Storage` enumerates the published
  runtime roots even though the parent volume has no node of those names —
  deduplicated against a same-named real node, rendered as a structural
  directory entry with the `UNIX_EPOCH` stamp any stampless backing
  reports (drives.md §10).
- **Provisioning:** the mkimage debug profile and the encrypted-root test
  fixtures seed `storage:100` and the seeded `root` account's membership;
  `useradd` keeps its coreutils `-G` surface (no hidden default), and the
  staged installer's account-creation step is where interactive users are
  added to the group.
- Host-proven end to end: the volume-service lifecycle test attaches a
  served FAT32 volume under the armed identity map and drives the
  production `fs_*` service as a member (mapped stat `0o664`/uid 0/gid
  100, read + write) and a non-member (write refused, other-class read
  allowed); the readdir merge, dedupe, and merged-name resolution are
  covered over the mock-backed mount service, `direct_children` and
  `GroupMappedFs` by unit tests, and the unlock test asserts the gid cell
  arms from the on-disk registry. The live path rides the Pi 4 metal
  acceptance with the D2/D3c precedent (no emulated fixture publishes USB
  nodes).

### 2.5 D4 — surprise removal, force-unmount, verified re-insert

Staged as three sub-increments, each green alone on the whole-project
gate (§7): the retention journal and the surprise-removal state machine
first, then the force-unmount exit, then the verified re-insert replay.

#### 2.5.1 D4a — retained writes + the surprise-removal state machine. **Done.**

- **The retention journal.** Every filesystem driver in the tree is
  write-through (`flush()` is a no-op), so the only bytes an unplug can
  lose are those the device accepted since its last committed flush.
  `kernel/core::fs::retained` holds exactly that set: `RetainedWrites`
  (per-LBA coalesced copies of written device blocks, bounded by the
  documented heap fraction — `CacheBudget::from_backing` — and gated per
  growth by `MemoryPressure::growth_permitted`; every buffer wiped on
  release) and `JournaledBlock`, the `Block` wrapper the attach path
  threads between the kernel blkio client and the partition window. It
  records successful writes, marks the journal **lost** on a failed
  write (the medium's state is unknown — the set may no longer be the
  complete delta), drops discarded ranges, and past a commit watermark
  (hard/16) issues the device flush (SCSI `SYNCHRONIZE CACHE`, via the
  new `FlushBlock` seam `BlkClient` implements) and empties the journal
  on success — the quiesce-time flush, and what keeps a long copy
  committing steadily instead of ballooning to the budget. A committed
  flush also resets a lost journal: nothing is uncommitted after it.
- **The unplug trigger** is endpoint teardown:
  `callreg::teardown_owned_by` (the one definition both the exit syscall
  and the driver-store teardown call) destroys the dead task's
  endpoints, wakes the cancelled callers, then notifies the set-once
  `EndpointVanishObserver` — the volume service, installed at boot next
  to `VOLUME_SERVICE.install`. Ordering is load-bearing: the observer
  runs strictly **after** `call_wake`, so a caller parked mid-call on
  the dead endpoint can finish and release any lock the observer needs.
- **The transitions** (`RuntimeVolumeService::handle_vanished`,
  idempotent and non-parking — the attached-volume registry moved behind
  a `SpinLock`, with a separate `SleepLock` serialising whole
  attach/detach operations): clean journal → the volume simply retracts
  (unmount, unregister, unpublish; event 4176 — no drama, drives.md
  §10); dirty → **`unavailable-dirty`** (event 4177 with the retained
  byte count); retention abandoned → **`unavailable-lost`** (event
  4178, "uncommitted data existed and was not retained"). An unavailable
  volume's registry slot is re-pointed at the fail-closed
  `UnavailableFs` stand-in, so **every** operation — including reads
  `CachedFs` could otherwise have served from plaintext cache — reports
  `DeviceFault`, while the mount, alias, and `id::` root stay visible.
  A plain `volume_detach` of an unavailable volume is refused
  (`volume_unavailable_{dirty,lost}`): discarding the retained set is
  D4b's audited force-unmount, never an implicit side effect. drives.md
  §23 carries the new `fs.hotplug.surprise_removal.{clean,dirty,lost}`
  events.
- **Defects fixed en route (§2.18), each with a regression test:**
  `VfsError::Io` mapped to `Errno::NotImplemented`, misreporting a dead
  device as "interface intentionally inert" — now `Errno::DeviceFault`;
  and `Fat32::format` wrote a **zero BPB volume serial**, so any two
  RustOS-formatted FAT32 volumes shared one content-derived identity and
  the second could never attach while the first was mounted — `format`
  now takes a caller-minted serial and refuses zero (the ext4
  nil-`s_uuid` precedent; `tools/mkimage`'s boot partition uses a fixed,
  documented serial so images stay bit-reproducible — that FAT is never
  published in the volume forest).
- Host-proven: the `retained` unit suite (coalescing, watermark commit,
  refused flush keeps the set, budget/pressure exhaustion → lost, failed
  write → lost, commit resets lost, discard drops its range) and two new
  lifecycle scenarios over served volumes — a dirty unplug (root stays
  published, reads fail `DeviceFault` cache included, plain detach
  refused) and a clean unplug (root retracted, mount gone, nothing left
  to detach). Live path: Pi 4 metal acceptance (QEMU models no Pi USB —
  the D2/D3 precedent).
- **Deliberate staging.** The user-facing session notification (§2.24)
  is not emitted yet: no system notification service exists to carry it
  (the taskbar's notification area is an in-process GUI model, not an
  IPC surface), so the syslog/audit record is the D4a channel and the
  notification emit lands with the session-notification surface when one
  exists. sysinfo `MOUNT_LIST` does not yet mark availability; it joins
  D4b, whose force-unmount tooling needs the observable state.

#### 2.5.2 D4b — force-unmount

- `unmount --force <name>` (extending the existing mount tooling,
  coreutils-adjacent spelling per §16.7) discards the retained set,
  unpublishes the root, and logs the deliberate data loss with its own
  event id (4179 is reserved next to the D4a events). Capability-gated
  to the volume's mount authority (`CAP_FS_MOUNT`); fails closed
  otherwise.
- The ABI evolves in place (§2.13 — `abi-v1` is unfrozen):
  `VolumeDetachRequest` gains the force flag, and the kernel detach path
  distinguishes the audited force-discard of an unavailable volume from
  the flush-first clean detach.
- sysinfo `MOUNT_LIST` gains the availability mark so the mount tooling
  can show `unavailable-dirty`/`unavailable-lost` rather than a volume
  that looks healthy.

#### 2.5.3 D4c — verified re-insert

- On re-attach, `volmgr` matches the new volume against each
  `unavailable-dirty` record by durable identity (volume UUID/id) **and
  proves non-mutation before replaying**: the filesystem driver compares
  its mutation evidence — RustFS generation/root checksum; ext4
  superblock write-time/mount-count/checksums; FAT32 FSInfo + a bounded
  re-read comparison of the exact regions the retained writes depend on
  (declared per driver through the filesystem capability API, honestly
  weaker for weaker formats). Provably unmutated → the retained writes
  replay, the volume returns to service, and the recovery is logged. Any
  doubt → fail closed: the volume mounts fresh and
  read-only-until-acknowledged, the retained set is kept (budget
  permitting) for explicit salvage or `--force` discard, and the
  conflict is logged. Never silently merge (§5.4, §26.5).
- Tests: host simulations of unplug-with-dirty-data (retain → replay on
  identical image; retain → refuse on mutated image; force-unmount
  discard), each with its syslog assertion; a QEMU vertical driving
  detach/re-attach of a `usb-storage` image where the emulated path
  exists.

### 2.6 DEVICE2 increment order

- **D1** bulk URB transport (host-provable alone). **Done** (§2.2).
- **D2** `usb_msd` class driver (host mock + QEMU/metal). **Done** (§2.3).
- **D3a** durable `id::` roots — `lib/path` `Root::VolumeId` + the kernel
  volume forest + boot-volume publication. **Done** (§2.4.1).
- **D3b** runtime volume attach over blkio + multi-root
  publish/unpublish. **Done** (§2.4.2).
- **D3c** `volmgr` — the per-node automount policy driver + `lib/fsprobe`
  + deterministic naming. **Done** (§2.4.3).
- **D3d** mount-policy permissions (`storage` group identity map) + the
  `Storage:` catalog enumeration. **Done** (§2.4.4).
- **D4a** retained uncommitted writes + the surprise-removal state
  machine. **Done** (§2.5.1).
- **D4b** force-unmount (`unmount --force`, the detach force flag, the
  sysinfo availability mark).
- **D4c** verified re-insert (mutation evidence + retained-write replay).

D3 is deliberately after D2 so automount is proven against a real
hot-pluggable block source, but its volume-forest core is bus-neutral
and serves the existing virtio/emmc disks identically — nothing in
`volmgr`'s engine names a bus; its bind table selects block-service
nodes by their own compatible keys, which is data, not bus coupling
(§2.20).

---

## 3. Out of scope (explicitly)

- Non-SCSI mass-storage transports (UAS, CBI) and non-disk SCSI types
  (tape, optical) — later class-driver extensions on the same seams.
- Multi-tier USB hubs (unchanged from `plans/USB.md` §4).
- Writing the hardware-tree subsystem-id extension for `lspci -v`
  parity — a future `hwtree` ABI revision with its own consumer.
- An offline `update-pciids`-style on-target database refresher: the
  database updates with system updates through the signed image path
  (§19.3 forbids the OS fetching data-that-becomes-output from the
  network at runtime without the update path's signing).
