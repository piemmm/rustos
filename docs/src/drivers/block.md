# Block drivers

Block drivers expose a fixed-size logical-block array to higher layers
(filesystems, swap, dump). They implement
[`rustos_abi::driver::block::Block`](../abi/driver_traits.md) and are
loaded as user-space drivers unless their manifest declares
`kind = "in-kernel"` (in which case they require `CAP_DRV_KERNEL`).

## Class trait

`Block` exposes three method families:

| Method                         | Purpose                                       | Capability gate                |
|--------------------------------|-----------------------------------------------|--------------------------------|
| `geometry`                     | report `BlockGeometry { block_size, block_count }` | `DriverHandle` ownership |
| `read_blocks` / `write_blocks` | bulk transfer (multiple of `block_size`)      | `DriverHandle` ownership       |
| `read_blocks_with_class` / `write_blocks_with_class` | classed transfer (see below) | `DriverHandle` ownership |

All methods return `Result<_, DriverError>`. Per `AGENTS.md` §2.9 the
class trait never panics — buffer length errors map to
`DriverError::BufferTooSmall`, out-of-range LBAs map to
`DriverError::LengthOutOfRange`, and device-reported failures map to
`DriverError::DeviceFault`.

## `BufferClass` and zero-on-free

`*_with_class` accept a `BufferClass` (`NonSensitive` /
`Sensitive`). Per `AGENTS.md` §4 a driver that bounces payload
through an internal staging area **must** scrub that staging before
the method returns when `class == Sensitive`. The default
implementations of the `_with_class` methods delegate to the plain
methods and are only safe for drivers that DMA straight into the
caller-owned buffer; drivers that bounce-buffer (such as
`virtio_blk` over the Stage 4 host-side allocator) override them.

The trait makes no guarantee about scrubbing the caller-owned `buf`;
that remains the caller's responsibility once it has consumed the
payload.

## Sharing one device across windows

The boot path brings up exactly **one** bootstrap-floor block device, yet two
independent consumers must read it during bring-up — the read-only signed
`/System` driver-store mount and the encrypted-root unlock window — and, under
Design D, the `/System` store must stay reachable for on-demand and reactive
(hotplug) driver loads (`AGENTS.md` §18.3 / §18.4). One disk must therefore
back two concurrent partition windows.

The kernel block-sharing layer (`rustos_kernel::shared_block`) is that
primitive. A `SharedBlock<B>` owns the brought-up device behind a `lib/sync`
`SpinLock` and hands out `SharedBlockHandle`s, each of which is itself a
`Block`. Every byte-moving operation takes the lock for the duration of one
device call, so concurrent windows on different CPUs are serialised
(`AGENTS.md` §4 — SMP from day one). The device's `BlockGeometry` is immutable
for the life of a disk, so it is queried once at construction and cached:
`geometry()` is then lock-free (`AGENTS.md` §2.16). A geometry fault at
construction refuses to wrap the device, so no handle is ever handed out for
an unusable device (fail closed, §2.9).

A plain `SpinLock` (not the IRQ-safe variant) is correct because block I/O is
driven from task / kthread context — the device IRQ only *wakes* the waiting
kthread, it never issues a transfer from inside the handler — so the lock is
never taken from an interrupt. The layer is generic over any `Block` and names
no device or architecture, so every port shares the one definition (§2.2 /
§2.20). The aarch64 root-unlock tail (`finish_unlock`) wraps its brought-up
virtio-blk or EMMC2 device in a `SharedBlock` and drives both the `/System`
autoload and the interactive unlock through concurrent handles rather than
borrowing then moving the one device.

## The persistent driver-store service

Design D needs the `/System` driver store reachable for the life of the system
(on-demand and reactive driver loads, `AGENTS.md` §18.3 / §18.4), not only
during boot. `DriverStoreService<B>` (`rustos_kernel::shared_block`) owns the
boot disk's `SharedBlock` and hands out a fresh read-only window
(`SharedBlockHandle`) for each `/System` read.

It keeps the mount alive **without promoting the device backing to `'static`**.
The aarch64 root-unlock kthread is a *never-returning* kernel service
(`AGENTS.md` §17.1 — "a continuous service never returns"): because
`finish_unlock` receives the brought-up device by value while its backing (the
DMA pool, MMIO map, IRQ waiter, and virtio host, or the EMMC2 register-window
map) stays on the still-suspended `virtio_blk_unlock` / `emmc2_unlock` frame,
making `finish_unlock` never return keeps that whole bring-up call chain
suspended on the kthread's coroutine stack. The borrowed backing therefore
stays live for free, and the proven IRQ-wait / cooperative-yield device-driving
model is unchanged (`AGENTS.md` §2.17 — no security or correctness regression
on a metal-confirmed path).

After running the boot autoload and the encrypted-root unlock through two
concurrent windows, logging the outcome, and releasing the console-0 gate to
`login`, the service calls `DriverStoreService::hold`, which **parks** the
kthread for life owning the `SharedBlock` — a real park, never a busy-yield
loop (`AGENTS.md` §2.1), so it consumes no CPU while idle. A later reader (the
D2b `driver_store_load` path) wakes this kthread to serve a `/System` read
through a window and then re-parks, reusing the one proven I/O path rather than
driving the device from an arbitrary caller's context.

## Shipped drivers

| Driver                                   | Crate                                | Supported buses     | Status                                   |
|------------------------------------------|--------------------------------------|---------------------|------------------------------------------|
| [virtio-blk](./virtio.md)                | `rustos-drv-storage-virtio-blk`      | virtio (PCI / MMIO) | host-side tests + mock transport only    |
| Raspberry Pi 4 EMMC2                      | `rustos-drv-storage-emmc2`           | Pi 4 SDHCI (MMIO)   | read + write host-tested; interrupt-driven; wired into root-unlock; metal acceptance pending (Pi 4) |
| USB mass storage (BOT / CBI / UAS)        | `rustos-drv-storage-usb-msd`         | any USB host via the URB transport | shared SCSI layer + three wire transports (incl. UFI floppies) host-tested over scripted doubles; metal acceptance pending (Pi 4) |

QEMU integration on real PCI / MMIO virtio devices depends on the
prerequisites enumerated in `.junie/next-session-prompt.md` (kernel
DMA, IRQ routing, bus-handle hand-off).

### Discovery and the bootstrap floor

Every shipped block driver publishes a canonical `BIND_KEYS` table
(`AGENTS.md` §18.3) so a discovered hardware-tree node binds them by
match, never by a kernel guess (§18.5):

| Driver       | `BIND_KEYS` match key                         | Discovered node source                          |
|--------------|-----------------------------------------------|-------------------------------------------------|
| virtio-blk   | virtio device id `2` (`HwMatchKey::virtio(2)`)| a probed virtio node (PCI or MMIO transport)    |
| EMMC2        | `compatible = "brcm,bcm2711-emmc2"`           | the aarch64 `FdtDiscovery` Storage node         |
| USB MSD      | USB class `08:06:50` (`HwMatchKey::usb(0, 0, 0x08_06_50)`) | the mass-storage interface node the xHCI HCD emits |

The virtio-blk and EMMC2 drivers are part of the **bootstrap floor** (`AGENTS.md`
§18.6): the storage path must be up before the signed driver store under
`/System/Drivers/` is reachable, so the volume that holds the store can be
read. They are therefore compiled in and registered in the kernel binary's
`driver_catalog::IN_KERNEL_DRIVERS` floor registry (virtio-blk for the QEMU
`virt` / x86_64 root, EMMC2 for the Raspberry Pi 4 SD card), each paired
with the driver crate's own `BIND_KEYS` and a build-signed manifest. The
floor binds by discovery-match through the same shared `lib/devmatch`
policy the user-space `devmgr` uses — the in-kernel match and the
user-space match can never diverge (§2.2) — and is signature-verified and
capability-gated alike (§18.6). The floor only ever shrinks toward the
store, never grows.

### Raspberry Pi 4 EMMC2 (SDHCI)

`rustos-drv-storage-emmc2` brings up the Pi 4 (BCM2711) EMMC2
controller — an Arasan / SDHCI-5.1 SD host — and exposes the card
through `Block`. The transfer path is programmed-I/O: the SDHCI
command/response and block-transfer state machine moves one 512-byte
block at a time through the buffer data port in both directions
(`CMD17`/`CMD18` reads, `CMD24`/`CMD25` writes), so neither path needs
a DMA capability (`plans/PI.md` P8).

The state machine (`Emmc2`) is written against the `SdhciHost` register
seam, so it is proven host-side against a register-level mock controller
and runs on metal over a capability-gated `RegisterWindow` mapped by
`wiring::open_discovered` from the device-tree-discovered
`brcm,bcm2711-emmc2` node (`AGENTS.md` §2.2 / §18.3). There is no
Pi-board QEMU vertical (QEMU does not model EMMC2, `plans/PI.md` §0.4);
the emulation artefact is the host test and metal acceptance is the
documented bring-up checklist. `Emmc2::open` runs the standard SD
identification (`CMD0`/`CMD8`/`ACMD41`/`CMD2`/`CMD3`/`CMD9`/`CMD7`/`CMD16`)
and derives geometry from the card CSD; only high-capacity,
block-addressed (SDHC/SDXC, CSD v2) cards are supported and anything
else is rejected fail-closed.

Identification runs at the SD identification clock (≤400 kHz) on the 1-bit
bus the controller resets to. Once the card is selected, two pure speed
steps run before any block transfer: `ACMD6` switches the card to the
4-bit bus (the controller's `CONTROL0` data-width bit set to match, 4×),
and the SD clock is raised to the data divisor (`DATA_CLOCK_DIVISOR`,
derived as `IDENT_CLOCK_DIVISOR / 32` so the data clock is 32× the
identification clock — ≤12.8 MHz, within SD Default Speed's 25 MHz, no
high-speed switch needed). This turns the ~50 KB/s identification-clock
1-bit path into the ~6 MB/s Default-Speed 4-bit path the driver-store scan
and every bundle read inherit (`AGENTS.md` §2.16); the divisor is derived
from the identification divisor, not a base-clock constant, so it carries
no board assumption (`AGENTS.md` §2.20).

Command- and transfer-completion waits **park on the controller's
interrupt** through a `CompletionWait` seam (`SdhciHost::await_irq`) rather
than busy-spinning a status register, so a slow SD operation never
monopolises the CPU and starves interrupt-driven work (`AGENTS.md` §17.1 /
§2.16) — the defect that froze the boot UART log while `/System` was being
read during driver autoload. `reset_and_clock` enables the controller's
completion-signal sources (`IRPT_EN`) so it raises its CPU interrupt line
on each completion and on every error bit; the kernel supplies the
`CompletionWait` that binds, routes, arms, and parks on that GIC line
(`emmc2_unlock`, below). The remaining identification-only register
handshakes that have no completion source (reset, clock-stable) still spin,
and every wait is bounded by a poll budget that fails closed with
`DriverError::DeviceFault` rather than waiting forever (`AGENTS.md` §2.1).

Bring-up resets the host controller and then **powers the card rail**
(SD Bus Power on, 3.3 V) through the power-control byte of `CONTROL0`
*before* clocking the bus. The full host-controller reset clears SD Bus
Power, and the standard SDHCI register block gates all command/data
activity on it, so without this write the very first command (`CMD0`)
never completes (the bus is dark) — the failure a real Pi 4 reported at
`stage=CMD0 GO_IDLE_STATE`. Linux's Pi 4 EMMC2 brings the same power
register up to `0x0F`.

The CSD geometry decode reads the R2 response **exactly as the controller
lays it out**: for a 136-bit response the SDHCI block strips the 8-bit CRC
tail and right-aligns the remaining 120 bits across `RESP0..3`, so
`CSD_STRUCTURE` (CSD[127:126]) lands at `RESP3` bits [23:22] — not the top
of the word, whose high byte is zero padding — and `C_SIZE` (CSD v2) at
`RESP1` bits [29:8]. Reading the structure field at the wrong position made
a real Pi 4's valid SDHC card decode as an unsupported structure and fail
at `stage=CMD9 SEND_CSD`; the decoder now reads the correct bits, and the
host mock models the same right-aligned layout so the regression cannot
recur.

Because there is no Pi-board QEMU vertical, the only signal that localises
a bring-up failure on a real Pi 4 is the UART log. `Emmc2::open` therefore
fails with a `BringUpFault` that pairs the underlying `DriverError` with a
`BringUpStage` naming the exact SD-identification step that stalled (map
register window, reset + SD clock, `CMD0`, `CMD8`, `ACMD41`, `CMD2`, `CMD3`,
`CMD9`, `CMD7`, `CMD16`, `ACMD6` set-bus-width, raise SD clock). A consumer
that only needs the §8 `DriverError`
drops the stage with `?` / `DriverError::from`; the in-kernel root-unlock
path instead logs `BringUpStage::as_str` as a structured `stage=` field
(`AGENTS.md` §2.16 — measure, do not guess).

The driver is **wired into the root-unlock path** (`plans/PI.md` B4): when
the root-storage bind gate binds the `brcm,bcm2711-emmc2` node, the aarch64
root-unlock kthread (`crate::aarch64::root_unlock::emmc2_unlock`) maps the
node's sole SDHCI register window under `CAP_MMIO_MAP` through a minimal
in-kernel MMIO-only DriverHost, admits the driver through the signed §8
load gate, **discovers the controller's GIC SPI from the firmware device
tree (`emmc2_spi`) and binds, routes, and arms it on the published IRQ
table** — supplying the driver a `CompletionWait` (`Emmc2Completion`) that
blocks on that line through the same task-parking waiter the virtio
bring-up uses (`rustos_kernel_core::IrqParkWaiter`, §2.2): a syscall-context
wait parks its task off the run queue (woken by the ISR's `irq_wake`), a
boot-kthread wait takes the bounded race-free `wfi` fallback, and a
controller silent past the 2 s budget fails the transfer closed as
`DriverError::DeviceFault` — opens the card, and feeds
the resulting `Block` to the same mount + `/System` autoload +
interactive-unlock tail as virtio-blk (`finish_unlock`, §2.2). With no EMMC2
interrupt in the device tree the bring-up fails closed rather than parking
on a line that can never fire (`AGENTS.md` §2.9 / §18.4). On a bring-up
failure it logs the failing
`BringUpStage` as the `stage=` field of the `EventId(4139)` unlock-service
error line together with the underlying `DriverError` as an `error=` field,
so the metal UART log names both the SD command the card stalled at and how
it failed — distinguishing a controller/command fault (`error=device
fault`) from a decode rejection (`error=unsupported`) at the same step.
Since `raspi4b` cannot model EMMC2, that live bring-up is metal-gated; the
host test and the §0.9 metal checklist are the acceptance artefacts.

### USB mass storage (BOT / CBI / UAS) — `drivers/storage/usb_msd`

`rustos-drv-storage-usb-msd` is the first **discovered-tier, user-space**
block driver (`plans/DEVICES.md` D2/D5): a pure USB *class* driver `devmgr`
autoloads against the mass-storage interface node the xHCI host-controller
driver emits. It owns no register window, no DMA, and no IRQ — every
transfer rides the bus-agnostic URB transport (`lib/usb`), so the same
binary serves a disk behind any host controller that speaks it.

The driver reads the device's own configuration descriptor to derive the
interface number, wire transport, command set, and endpoints (never
assumed), then drives one transport-neutral SCSI command layer
(`src/scsi.rs` — the transparent set, or UFI's 12-byte padded CDBs and
`MODE SENSE(10)` for floppies) over the transport the device speaks:

- **Bulk-Only Transport 1.0** (`08:06:50`, `08:04:50`): each command
  wrapped in a CBW on bulk-OUT, the data phase over the bulk pair in
  bounded chunks, and the CSW validated field by field (signature, tag
  match, residue bound, status) — the device is hostile input. A stalled
  data phase falls through to the CSW; a stalled CSW read is retried once;
  a tag mismatch, corrupt CSW, or phase error runs the spec's Bulk-Only
  Mass Storage Reset and fails the command closed.
- **Control/Bulk/Interrupt 1.1** (`08:04:00`, the classic USB floppy):
  the 12-byte command block over the ADSC control-OUT data stage (a
  control STALL is the device's "command not accepted" answer, recovered
  in place by the URB layer), the data phase over the bulk pair, and the
  two-byte command-completion interrupt (UFI ASC/ASCQ, or the typed
  status spelling for non-UFI sets); a malformed or out-of-step
  completion runs the spec's Command Block Reset.
- **USB Attached SCSI** (`08:06:62`): the four Pipe-Usage-named bulk
  pipes with tag-checked Command / Read-Ready / Write-Ready / Sense IU
  sequencing (USB 2.0 non-stream operation) and in-band autosense; every
  IU is validated fail-closed — a foreign tag, wrong-direction ready IU,
  or lying sense length refuses the exchange. One command is in flight at
  a time (the block service is synchronous); queueing, task-management
  IUs, and SuperSpeed streams are the staged remainder (`plans/DEVICES.md`
  §3).

Per logical unit (`GET MAX LUN` for BOT, `REPORT LUNS` for UAS, exactly
one for CBI; up to 16) the bring-up runs `INQUIRY` (non-disk types are
skipped), a bounded ready drain (the sense consumed per failed attempt),
`READ CAPACITY(10)`/`(16)` with a fully validated geometry (power-of-two
block size 512–4096; the 16-byte form covers units past the 32-bit LBA
horizon), and the command set's write-protect bit — enforced driver-side
(`DriverError::PermissionDenied` before any byte reaches the device), not
merely reported.

Each ready LUN is published as a **storage-class hardware-tree node**
(compatible `rustos,usb-msd-lun`) carrying two grants: a block-service
call endpoint and a 32 KiB shared data window. Consumers drive the unit
with the fixed-frame `rustos_abi::blkio` protocol (`BlkRequest`:
geometry / read / write / flush; completions carry the geometry and the
read-only flag) — the same request-reply IPC shape as the URB transport,
served by the driver's wait-set loop (never a busy-poll). A hot-unplug
surfaces as the URB endpoint vanishing: the driver retracts its LUN nodes
and exits cleanly so a re-plug re-enumerates and reloads it. The engine,
descriptor reader, and block service are host-proven over scripted
doubles; the live path is Pi 4 metal acceptance (QEMU models no Pi USB).

### Volume manager (automount policy) — `drivers/storage/volmgr`

`rustos-drv-storage-volmgr` closes the hotplug loop (`plans/DEVICES.md`
D3c): it is the **policy driver** `devmgr` autoloads against each per-LUN
block-service node (compatible `rustos,usb-msd-lun`), one instance per
node, spawned with exactly that node's blkio endpoint + shared-window
grants — the same discovery/match/grant machinery every driver uses, so
no new kernel surface and no ambient authority (an instance can never
reach a sibling device's transport; the per-endpoint grant gates every
`ipc_call`).

The instance is a **read-only prober**: a fail-closed blkio `Block`
client (hostile geometry refused at connect, `write_blocks` refuses by
construction), the layout probe (whole-device filesystem signature first
— a superfloppy — else the GPT/MBR table via `lib/partition`, each
present partition's head probed by content through `lib/fsprobe`;
declared partition types are hints the probe ignores), and the
deterministic naming policy (the volume's own label sanitised through
the alias character rules, else `<fstype><n>`; a name collision appends
the volume-identity fingerprint, lengthened per retry, so re-inserting
the same volume re-derives the same name). Each recognised volume is
handed to the kernel through the `CAP_FS_MOUNT`-gated, audited
`volume_attach` syscall — the kernel re-validates the grants, extent,
and name, opens the filesystem itself, mounts under `/Storage/<name>`,
and publishes the durable `id::` root. The instance then exits `0`
(run-to-completion; the kernel-held mount outlives it), logging every
outcome with stable event ids (4180–4184). Removal handling (surprise
removal, retained dirty state, force-unmount, verified re-insert) is the
staged D4 work.

The blkio client, probe plan, and naming policy are host-proven over
scripted devices and synthetic disk images; the live path is Pi 4 metal
acceptance, following the `usb_msd` precedent.
