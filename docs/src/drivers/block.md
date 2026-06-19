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
| Raspberry Pi 4 EMMC2                      | `rustos-drv-storage-emmc2`           | Pi 4 SDHCI (MMIO)   | read + write host-tested; wired into root-unlock; metal-confirmed (Pi 4) |

QEMU integration on real PCI / MMIO virtio devices depends on the
prerequisites enumerated in `.junie/next-session-prompt.md` (kernel
DMA, IRQ routing, bus-handle hand-off).

### Discovery and the bootstrap floor

Both shipped block drivers publish a canonical `BIND_KEYS` table
(`AGENTS.md` §18.3) so a discovered hardware-tree node binds them by
match, never by a kernel guess (§18.5):

| Driver       | `BIND_KEYS` match key                         | Discovered node source                          |
|--------------|-----------------------------------------------|-------------------------------------------------|
| virtio-blk   | virtio device id `2` (`HwMatchKey::virtio(2)`)| a probed virtio node (PCI or MMIO transport)    |
| EMMC2        | `compatible = "brcm,bcm2711-emmc2"`           | the aarch64 `FdtDiscovery` Storage node         |

The block drivers are part of the **bootstrap floor** (`AGENTS.md`
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
else is rejected fail-closed. Every controller wait is bounded by a poll
budget and fails closed with `DriverError::DeviceFault` rather than
spinning (`AGENTS.md` §2.1).

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
`CMD9`, `CMD7`, `CMD16`). A consumer that only needs the §8 `DriverError`
drops the stage with `?` / `DriverError::from`; the in-kernel root-unlock
path instead logs `BringUpStage::as_str` as a structured `stage=` field
(`AGENTS.md` §2.16 — measure, do not guess).

The driver is **wired into the root-unlock path** (`plans/PI.md` B4): when
the root-storage bind gate binds the `brcm,bcm2711-emmc2` node, the aarch64
root-unlock kthread (`crate::aarch64::root_unlock::emmc2_unlock`) maps the
node's sole SDHCI register window under `CAP_MMIO_MAP` through a minimal
in-kernel MMIO-only DriverHost, admits the driver through the signed §8
load gate, opens the card, and feeds the resulting `Block` to the same
mount + `/System` autoload + interactive-unlock tail as virtio-blk
(`finish_unlock`, §2.2). On a bring-up failure it logs the failing
`BringUpStage` as the `stage=` field of the `EventId(4139)` unlock-service
error line together with the underlying `DriverError` as an `error=` field,
so the metal UART log names both the SD command the card stalled at and how
it failed — distinguishing a controller/command fault (`error=device
fault`) from a decode rejection (`error=unsupported`) at the same step.
Since `raspi4b` cannot model EMMC2, that live bring-up is metal-gated; the
host test and the §0.9 metal checklist are the acceptance artefacts.
