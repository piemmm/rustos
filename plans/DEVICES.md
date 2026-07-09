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
  today serves control and interrupt transfers only; `serve_urb` rejects
  bulk fail-closed. Mass storage is the first bulk consumer.
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
  `Resources/` (V2; `usb.ids.bin` stages in `lib/devids/tables/` until
  V3); the `fuzz_devids` harness covers both untrusted surfaces.
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
  tree copy, `cargo xtask devids` retargeted; `usb.ids.bin` stages in
  `lib/devids/tables/` until V3 moves it into `lsusb.app`). Enabling
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
  PCI-function target that carries the app store (the §16.7-style
  "where the emulated path exists" principle V3 already states).
- **V3 — `lsusb`.** As V2 for the USB view, including the topology
  render; QEMU vertical where the emulated bus path exists, metal
  acceptance on the Pi 4 chain otherwise.

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

### 2.2 D1 — bulk transfers on the URB transport

- `lib/abi/src/usb_urb.rs` already spells `UsbTransferType::Bulk`;
  `lib/usb` gains real bulk support: per-endpoint transfer rings in the
  xHCI engine (IN and OUT), queueing of multiple outstanding bulk URBs
  per direction, short-packet and stall handling (`CLEAR_FEATURE
  (ENDPOINT_HALT)` + ring recovery), and `serve_urb` validation extended
  to bulk (endpoint ownership, length within the shared buffer, direction
  legal — fail closed as today).
- The HCD stays a single wait-set event loop (§2.23); bulk completions
  are delivered asynchronously exactly like interrupt completions. Data
  still moves through the U3a2 shared-memory buffers with the HCD
  bounce-copying into its own DMA rings — the class driver continues to
  hold **zero** DMA authority (§5.4).
- Host-proven over the existing register-level mock (which grows bulk
  endpoints), including stall/short-packet/queue-depth regressions.

### 2.3 D2 — `drivers/storage/usb_msd` (Bulk-Only Transport class driver)

- New crate `drivers/storage/usb_msd/` (a storage-class leaf, vendor-
  neutral namespace, §8): binds an emitted USB interface node matched by
  `HwMatchKey::usb(0, 0, 0x08_06_50)` — mass storage, SCSI transparent
  command set, bulk-only transport. Holds only `CAP_SHM` /
  `CAP_IPC_ENDPOINT` / `CAP_LOG_EMIT` plus the storage-node emission
  right; no MMIO, no DMA, no IRQ (least privilege, §5.4).
- Implements USB BOT (CBW/CSW framing, tag matching, phase-error
  recovery via Bulk-Only Mass Storage Reset) and the SCSI transparent
  subset a disk needs: `INQUIRY`, `TEST UNIT READY`, `REQUEST SENSE`,
  `READ CAPACITY(10)`/`(16)`, `READ(10)`/`(16)`, `WRITE(10)`/`(16)`,
  `SYNCHRONIZE CACHE(10)`, `MODE SENSE(6)` (write-protect bit),
  `GET MAX LUN`. Every device-supplied field (CSW tags, residues, sense
  data, capacity, LUN count) is bounds-checked fail-closed — the device
  is hostile input (§5.4, §19).
- Exposes each LUN as a `rustos_abi::driver::block::Block` served over
  the same driver IPC shape the existing block drivers use, and emits one
  storage-class hardware-tree node per LUN (so the volume layer reacts to
  it exactly as to any other disk). Read-only media (write-protect) is
  declared on the node/block capability, enforced fail-closed.
- Transfers are chunked through a fixed shared-buffer window (the
  virtio_blk precedent): per-device cost is fixed, never a function of
  request length or volume size (§2.16, §26).
- Host unit tests over a mock URB transport (BOT framing, tag mismatch,
  stall recovery, short reads, write-protect, multi-LUN); QEMU vertical
  with `qemu-xhci` + `usb-storage` where the emulated controller path
  exists, Pi 4 metal acceptance otherwise (`plans/PI.md` §0.4).

### 2.4 D3 — the volume manager and automount

This lands the still-open volume forest (PLAN.md P4) as its centre; the
work is completed here, not stubbed around (§2.19).

- **`userland/system/volmgr`** (new service bundle) owns volume policy,
  as `devmgr` owns driver policy: it watches the hardware tree
  (`hw_tree_wait`) for storage-class nodes, probes partitions through
  `lib/partition`, probes each partition for a supported filesystem
  (RustFS / ext4 / FAT32 signatures, fail-closed probe order), and asks
  the kernel to attach the matched filesystem driver and publish the
  volume's durable **`id::<volume-id>` root** — the canonical identity
  (`docs/src/filesystem/drives.md`). The `id::` resolver `Root` variants
  land with this increment.
- **Alias + catalog publication.** For each published root, alias policy
  derives a human name and the `Storage:` catalog view updates
  (`Storage:/<Name>` → `<Name>:/`, drives.md §15). Naming:
  1. the volume's own label, sanitised through the alias character rules
     (`plans/ALIAS.md` §5.2 — anything else is dropped, an empty result
     falls through);
  2. else `<fstype><n>` (`fat1`, `ext1`, …).
  **Collision resolution is deterministic, never a coin-flip:** a name
  already published gets the volume-id short fingerprint appended
  (`Backup@7K2M` shorthand form, ALIAS.md §3.8), which is unique by
  construction; a second collision is impossible. Re-inserting the same
  volume re-derives the same name (stable identity), so a user's scripts
  keep working.
- **Permissions so logged-in users can use the data (§5.3, §16.3):**
  removable volumes mount `nosuid,nodev,noexec` by default; relaxation
  requires `CAP_FS_MOUNT_RELAX` and is audit-logged. Foreign filesystems
  with no owner model (FAT32) get a mount-policy identity map: files
  appear owned by the `storage` group with group read/write, so any
  logged-in member (the installer adds interactive users to it) can read
  and write without ambient authority; volumes with a real owner model
  (ext4, RustFS) keep their on-disk owners/modes/ACLs. Automount itself
  runs under `volmgr`'s `CAP_FS_MOUNT`; no new capability is minted
  unless an enforcement gap proves one is needed (§5.2 minimalism).
- Every publish/unpublish/deny is logged with a stable event id (§19.4).
- Host unit tests over mock block devices (probe order, label
  sanitisation, collision fingerprinting, policy maps); QEMU vertical:
  attach a disk image → assert the root, alias, catalog entry, and a
  user-scoped read/write through `fs_open`.

### 2.5 D4 — surprise removal, force-unmount, verified re-insert

- **Clean state first.** The filesystem layer tracks per-volume dirty
  state; `SYNCHRONIZE CACHE` is issued on quiesce. A volume with no
  uncommitted data at unplug simply unpublishes: alias marked
  unavailable, root retracted, one syslog event — no drama (drives.md
  §10.3).
- **Dirty surprise removal.** When the HCD retracts the interface node
  while uncommitted writes exist:
  1. the block layer fails in-flight I/O with a typed error (never a
     hang or unbounded retry, §26.5);
  2. the volume enters **`unavailable-dirty`**: the root and alias stay
     visible but fail closed for new I/O; the uncommitted write-back set
     is retained in RAM under a bounded, memory-pressure-aware budget
     (§26.3) — if the budget cannot be honoured the state degrades to
     `unavailable-lost` and says so;
  3. a syslog/audit event with a stable id records volume identity, the
     amount of unwritten data, and the retention outcome (§19.4), and a
     user-facing notification is emitted through the session's
     notification surface so the human learns immediately (§2.24).
- **Force unmount.** `unmount --force <name>` (extending the existing
  mount tooling, coreutils-adjacent spelling per §16.7) discards the
  retained set, unpublishes the root, and logs the deliberate data loss
  with its own event id. Capability-gated to the volume's mount
  authority; fails closed otherwise.
- **Verified re-insert.** On re-attach, `volmgr` matches the new volume
  against each `unavailable-dirty` record by durable identity (volume
  UUID/id) **and proves non-mutation before replaying**: the filesystem
  driver compares its mutation evidence — RustFS generation/root
  checksum; ext4 superblock write-time/mount-count/checksums; FAT32
  FSInfo + a bounded re-read comparison of the exact regions the retained
  writes depend on (declared per driver through the filesystem capability
  API, honestly weaker for weaker formats). Provably unmutated → the
  retained writes replay, the volume returns to service, and the recovery
  is logged. Any doubt → fail closed: the volume mounts fresh and
  read-only-until-acknowledged, the retained set is kept (budget
  permitting) for explicit salvage or `--force` discard, and the
  conflict is logged. Never silently merge (§5.4, §26.5).
- Tests: host simulations of unplug-with-dirty-data (retain → replay on
  identical image; retain → refuse on mutated image; budget exhaustion →
  `unavailable-lost`; force-unmount discard), each with its syslog
  assertion; a QEMU vertical driving detach/re-attach of a `usb-storage`
  image where the emulated path exists.

### 2.6 DEVICE2 increment order

- **D1** bulk URB transport (host-provable alone).
- **D2** `usb_msd` class driver (host mock + QEMU/metal).
- **D3** `volmgr` + `id::` roots + alias/catalog automount + permissions.
- **D4** surprise-removal state machine, force-unmount, verified
  re-insert.

D3 is deliberately after D2 so automount is proven against a real
hot-pluggable block source, but its volume-forest core is bus-neutral
and serves the existing virtio/emmc disks identically — nothing in
`volmgr` may name USB (§2.20).

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
