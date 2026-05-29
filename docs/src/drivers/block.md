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

## Shipped drivers

| Driver                                   | Crate                                | Supported buses     | Stage 4 status                          |
|------------------------------------------|--------------------------------------|---------------------|------------------------------------------|
| [virtio-blk](./virtio.md)                | `rustos-drv-storage-virtio-blk`      | virtio (PCI / MMIO) | host-side tests + mock transport only    |

QEMU integration on real PCI / MMIO virtio devices depends on the
prerequisites enumerated in `.junie/next-session-prompt.md` (kernel
DMA, IRQ routing, bus-handle hand-off).
