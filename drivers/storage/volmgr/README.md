# tairix-drv-storage-volmgr

The autoloaded user-space **volume-manager policy driver**
(`plans/DEVICES.md` D3c): the automount policy for hot-pluggable block
devices, owning volume policy the way `devmgr` owns driver policy.

## What it binds

The per-LUN **block-service storage node** a block driver emits
(compatible `tairix,usb-msd-lun` today; future hot-pluggable block
sources join by adding their node's compatible key to `BIND_KEYS`).
`devmgr` loads **one instance per matched node**; the kernel spawns it
holding exactly that node's two transport grants — the blkio call
endpoint and the shared data window — so an instance can probe and
publish only its own unit (no ambient authority). It knows neither the
bus nor the vendor behind the block service: every byte arrives over the
public `tairix_abi::blkio` protocol.

## What it does

1. Connects the read-only blkio client and validates the device geometry
   fail-closed (`src/blk.rs`).
2. Probes the layout (`src/plan.rs`): a whole-device filesystem
   signature first (superfloppy), else the partition table
   (`lib/partition`, GPT/MBR), probing each present partition's head
   with the shared signature probe (`lib/fsprobe`). Declared partition
   types are hints the probe ignores; the content signature decides.
3. Derives each recognised volume's deterministic catalog name
   (`src/name.rs`): the volume's own label sanitised through the alias
   character rules, else `<fstype><n>`; a name collision appends the
   volume-identity fingerprint, lengthened per retry — re-inserting the
   same volume re-derives the same name.
4. Asks the kernel to attach and publish each volume (the
   `CAP_FS_MOUNT`-gated, audited `volume_attach` syscall). The kernel
   re-validates the caller's grants, the extent, and the name, opens the
   filesystem itself, mounts under `/Storage/<name>`
   (`nosuid,nodev,noexec`, `ro` per the device), and publishes the
   durable `id::` root.
5. Exits `0` — publication is a run-to-completion job; the kernel-held
   mount outlives the instance, and a re-plug re-discovers the node and
   reloads the driver afresh.

## Required capabilities

`CAP_SHM` (map the granted data window), `CAP_IPC_ENDPOINT` (blkio calls
on the one granted endpoint), `CAP_FS_MOUNT` (the audited attach),
`CAP_HW_EMIT` (publish the array-member node for a device recognised as a
RAID member), and `CAP_LOG_EMIT` (diagnostics). No MMIO, DMA, or IRQ
authority. The kernel parents an emitted node to this driver's own matched
node and admits only resources this task already holds, so the emission can
republish this device's transport and nothing else.

## Limitations

- Removal is not handled here: surprise-removal state, retained dirty
  data, force-unmount, and verified re-insert are the staged D4 work
  (`plans/DEVICES.md` §2.5). A cleanly detached volume is withdrawn
  through `volume_detach` by that machinery, not by this driver.
- Only the kernel-attachable filesystems (ARXFS / ext4 / FAT32) are
  recognised; anything else is left untouched and logged.
- The probe is read-only by construction (`write_blocks` refuses), so a
  hostile or corrupt medium can never be mutated by probing it.

Host tests cover the blkio client, the probe plan, and the naming policy
over scripted devices and synthetic disk images; the live path is Pi 4
metal acceptance (QEMU models no Pi USB), following the `usb_msd`
precedent.
